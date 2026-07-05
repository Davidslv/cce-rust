import os

def read_config(path):
    return os.path.join(path, "config.yml")

class Loader:
    def load(self):
        return read_config(".")
