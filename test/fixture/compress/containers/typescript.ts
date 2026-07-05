export class Service {
  private repo: Repo;
  limit: number = 10;
  find(id: string): Case {
    return this.repo.find(id);
  }
}
