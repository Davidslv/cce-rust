class Service {
  count = 0;
  find(id) {
    return this.repo.find(id);
  }
  save(x) {
    this.store.push(x);
  }
}
