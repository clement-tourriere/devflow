const STORAGE_KEY = "devflow:project-last-accessed";

function load(): Record<string, number> {
  try {
    return JSON.parse(localStorage.getItem(STORAGE_KEY) || "{}");
  } catch {
    return {};
  }
}

function save(data: Record<string, number>) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(data));
}

export function recordProjectAccess(path: string) {
  const data = load();
  data[path] = Date.now();
  save(data);
}

export function getLastAccessed(path: string): number {
  return load()[path] ?? 0;
}

export function sortByRecent<T>(items: T[], getPath: (item: T) => string): T[] {
  const data = load();
  return [...items].sort(
    (a, b) => (data[getPath(b)] ?? 0) - (data[getPath(a)] ?? 0)
  );
}
