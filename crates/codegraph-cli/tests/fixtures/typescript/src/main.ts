import { doThing } from '@app/lib';
import { Service } from './svc';

export function go(): number {
  const s = new Service();
  return doThing() + s.run();
}
