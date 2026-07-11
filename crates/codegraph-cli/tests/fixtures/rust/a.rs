pub fn helper() -> i32 { 1 }
pub fn local_target() -> i32 { 2 }
pub fn same_file_caller() -> i32 { local_target() }
