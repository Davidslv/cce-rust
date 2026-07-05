class Service:
    """A tiny service over a repo."""
    def find(self, id):
        return self.repo.find(id)

    def save(self, x):
        self.store.append(x)
