import { Service } from './svc';

export const Header = () => { return titleOf(); };

export function titleOf(): string { return "t"; }

export const Page = () => {
  const svc = new Service();
  svc.run();
  return Header();
};
