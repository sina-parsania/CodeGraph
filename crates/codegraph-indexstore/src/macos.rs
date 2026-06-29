//! libIndexStore FFI reader (macOS). Uses the `_apply_f` function-pointer variants
//! (the block `_apply` variants are Obj-C blocks Rust can't call). Build-time linked
//! against the toolchain's `libIndexStore.dylib` (see build.rs). C signatures
//! verified against indexstore.h.

#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::path::Path;
use std::ptr;

use super::{Occ, ROLE_CALL, ROLE_DEFINITION, ROLE_REL_CALLEDBY};

#[repr(C)]
#[derive(Clone, Copy)]
struct StringRef {
    data: *const c_char,
    length: usize,
}

type Store = *mut c_void;
type ErrorT = *mut c_void;
type UnitReader = *mut c_void;
type RecordReader = *mut c_void;
type UnitDep = *mut c_void;
type Occurrence = *mut c_void;
type Symbol = *mut c_void;
type SymbolRelation = *mut c_void;

type StrApplier = extern "C" fn(*mut c_void, StringRef) -> bool;
type DepApplier = extern "C" fn(*mut c_void, UnitDep) -> bool;
type OccApplier = extern "C" fn(*mut c_void, Occurrence) -> bool;
type RelApplier = extern "C" fn(*mut c_void, SymbolRelation) -> bool;

const DEP_KIND_RECORD: c_int = 2;

extern "C" {
    fn indexstore_store_create(path: *const c_char, error: *mut ErrorT) -> Store;
    fn indexstore_store_dispose(s: Store);
    fn indexstore_error_get_description(e: ErrorT) -> *const c_char;
    fn indexstore_store_units_apply_f(s: Store, sorted: c_uint, ctx: *mut c_void, f: StrApplier) -> bool;
    fn indexstore_unit_reader_create(s: Store, name: *const c_char, error: *mut ErrorT) -> UnitReader;
    fn indexstore_unit_reader_dispose(r: UnitReader);
    fn indexstore_unit_reader_get_main_file(r: UnitReader) -> StringRef;
    fn indexstore_unit_reader_dependencies_apply_f(r: UnitReader, ctx: *mut c_void, f: DepApplier) -> bool;
    fn indexstore_unit_dependency_get_kind(d: UnitDep) -> c_int;
    fn indexstore_unit_dependency_get_name(d: UnitDep) -> StringRef;
    fn indexstore_unit_dependency_get_filepath(d: UnitDep) -> StringRef;
    fn indexstore_record_reader_create(s: Store, name: *const c_char, error: *mut ErrorT) -> RecordReader;
    fn indexstore_record_reader_dispose(r: RecordReader);
    fn indexstore_record_reader_occurrences_apply_f(r: RecordReader, ctx: *mut c_void, f: OccApplier) -> bool;
    fn indexstore_occurrence_get_symbol(o: Occurrence) -> Symbol;
    fn indexstore_occurrence_get_roles(o: Occurrence) -> u64;
    fn indexstore_occurrence_get_line_col(o: Occurrence, line: *mut c_uint, col: *mut c_uint);
    fn indexstore_symbol_get_usr(sym: Symbol) -> StringRef;
    fn indexstore_occurrence_relations_apply_f(o: Occurrence, ctx: *mut c_void, f: RelApplier) -> bool;
    fn indexstore_symbol_relation_get_roles(r: SymbolRelation) -> u64;
    fn indexstore_symbol_relation_get_symbol(r: SymbolRelation) -> Symbol;
}

unsafe fn decode(s: StringRef) -> String {
    if s.data.is_null() || s.length == 0 {
        return String::new();
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(s.data as *const u8, s.length)).into_owned()
}

fn rel_path(root: &Path, abs: &str) -> String {
    Path::new(abs).strip_prefix(root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| abs.to_string())
}

extern "C" fn unit_cb(ctx: *mut c_void, name: StringRef) -> bool {
    unsafe { (*(ctx as *mut Vec<String>)).push(decode(name)) };
    true
}

extern "C" fn dep_cb(ctx: *mut c_void, dep: UnitDep) -> bool {
    unsafe {
        if indexstore_unit_dependency_get_kind(dep) == DEP_KIND_RECORD {
            let name = decode(indexstore_unit_dependency_get_name(dep));
            let file = decode(indexstore_unit_dependency_get_filepath(dep));
            if !name.is_empty() {
                (*(ctx as *mut Vec<(String, String)>)).push((name, file));
            }
        }
    }
    true
}

struct RelCtx {
    caller: Option<String>,
}
extern "C" fn rel_cb(ctx: *mut c_void, rel: SymbolRelation) -> bool {
    unsafe {
        if indexstore_symbol_relation_get_roles(rel) & ROLE_REL_CALLEDBY != 0 {
            let usr = decode(indexstore_symbol_get_usr(indexstore_symbol_relation_get_symbol(rel)));
            if !usr.is_empty() {
                (*(ctx as *mut RelCtx)).caller = Some(usr);
                return false; // first CALLEDBY is the caller; stop
            }
        }
    }
    true
}

struct OccCtx<'a> {
    occs: &'a mut Vec<Occ>,
    file: &'a str,
}
extern "C" fn occ_cb(ctx: *mut c_void, occ: Occurrence) -> bool {
    unsafe {
        let oc = &mut *(ctx as *mut OccCtx);
        let roles = indexstore_occurrence_get_roles(occ);
        let is_def = roles & ROLE_DEFINITION != 0;
        let is_call = roles & ROLE_CALL != 0;
        if !is_def && !is_call {
            return true;
        }
        let usr = decode(indexstore_symbol_get_usr(indexstore_occurrence_get_symbol(occ)));
        if usr.is_empty() {
            return true;
        }
        let (mut line, mut col): (c_uint, c_uint) = (0, 0);
        indexstore_occurrence_get_line_col(occ, &mut line, &mut col);
        let _ = col; // line/col both written by the C call; we only need line
        if is_def {
            oc.occs.push(Occ { usr: usr.clone(), roles: ROLE_DEFINITION, file: oc.file.to_string(), line, caller_usr: None });
        }
        if is_call {
            let mut rc = RelCtx { caller: None };
            indexstore_occurrence_relations_apply_f(occ, &mut rc as *mut _ as *mut c_void, rel_cb);
            oc.occs.push(Occ { usr, roles: ROLE_CALL, file: oc.file.to_string(), line, caller_usr: rc.caller });
        }
    }
    true
}

pub fn read_occurrences(store_path: &Path, root: &Path) -> anyhow::Result<Vec<Occ>> {
    let cpath = CString::new(store_path.to_str().unwrap_or_default())?;
    let mut err: ErrorT = ptr::null_mut();
    let store = unsafe { indexstore_store_create(cpath.as_ptr(), &mut err) };
    if store.is_null() {
        let msg = if err.is_null() {
            "unknown error".to_string()
        } else {
            unsafe { CStr::from_ptr(indexstore_error_get_description(err)).to_string_lossy().into_owned() }
        };
        anyhow::bail!("indexstore_store_create failed: {msg}");
    }

    let mut units: Vec<String> = Vec::new();
    unsafe { indexstore_store_units_apply_f(store, 0, &mut units as *mut _ as *mut c_void, unit_cb) };

    let mut occs: Vec<Occ> = Vec::new();
    for unit_name in &units {
        let Ok(cu) = CString::new(unit_name.as_str()) else { continue };
        let mut e2: ErrorT = ptr::null_mut();
        let reader = unsafe { indexstore_unit_reader_create(store, cu.as_ptr(), &mut e2) };
        if reader.is_null() {
            continue;
        }
        let main_file = unsafe { decode(indexstore_unit_reader_get_main_file(reader)) };
        let mut records: Vec<(String, String)> = Vec::new();
        unsafe {
            indexstore_unit_reader_dependencies_apply_f(reader, &mut records as *mut _ as *mut c_void, dep_cb);
            indexstore_unit_reader_dispose(reader);
        }
        for (rname, rfile) in &records {
            let abs = if rfile.is_empty() { main_file.as_str() } else { rfile.as_str() };
            // Skip dependency/system records — a real IndexStore also indexes every
            // SwiftPM checkout + SDK framework (≈19M occurrences); only files under
            // the repo can map to a node, so filter them out early (huge speedup).
            if !Path::new(abs).starts_with(root) {
                continue;
            }
            let file = rel_path(root, abs);
            let Ok(cr) = CString::new(rname.as_str()) else { continue };
            let mut e3: ErrorT = ptr::null_mut();
            let rr = unsafe { indexstore_record_reader_create(store, cr.as_ptr(), &mut e3) };
            if rr.is_null() {
                continue;
            }
            let mut oc = OccCtx { occs: &mut occs, file: &file };
            unsafe {
                indexstore_record_reader_occurrences_apply_f(rr, &mut oc as *mut _ as *mut c_void, occ_cb);
                indexstore_record_reader_dispose(rr);
            }
        }
    }
    unsafe { indexstore_store_dispose(store) };
    Ok(occs)
}
