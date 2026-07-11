export class Service {
  run(): number { return this.step(); }
  step(): number { return 1; }
}
export class Controller {
  constructor(private svc: Service) {}
  handle(): number { return this.svc.run(); }
}
