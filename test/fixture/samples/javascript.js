import fs from "fs";

function readConfig(path) {
  return fs.readFileSync(path);
}

class Loader {
  load() {
    return readConfig(".");
  }
}
