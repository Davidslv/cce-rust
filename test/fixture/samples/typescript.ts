import { readFile } from "fs";

export function loadConfig(path: string): string {
  return readFile(path);
}

export interface Config {
  name: string;
}

export class Loader {
  load(): Config {
    return { name: loadConfig(".") };
  }
}
