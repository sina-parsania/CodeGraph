class Svc {
    void run() {}
}
class App {
    Svc svc;
    void main() { svc.run(); helper(); }
    void helper() {}
}
