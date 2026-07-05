// A tsconfig.json marks this package.json member as typescript (detection rule 3).
export function render(title: string): string {
  return `<h1>${title}</h1>`;
}
