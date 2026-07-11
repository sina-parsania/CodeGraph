class Engine {
    func start() { self.ignite() }
    func ignite() {}
}
class Car {
    let engine: Engine = Engine()
    func drive() { engine.start() }
}
