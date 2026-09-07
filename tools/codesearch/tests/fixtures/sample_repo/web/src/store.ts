/** 前端运行时配置。 */
export interface Settings {
  endpoint: string;
  maxAttempts: number;
  baseDelayMs: number;
}

const DEFAULT_SETTINGS: Settings = {
  endpoint: "http://127.0.0.1:8080",
  maxAttempts: 3,
  baseDelayMs: 200,
};

/** 读取配置，缺失字段回退到默认值。 */
export function loadSettings(overrides: Partial<Settings> = {}): Settings {
  return { ...DEFAULT_SETTINGS, ...overrides };
}

/** 简单的内存缓存，用于避免重复请求。 */
export class MemoryCache<T> {
  private readonly entries = new Map<string, T>();

  get(key: string): T | undefined {
    return this.entries.get(key);
  }

  set(key: string, value: T): void {
    this.entries.set(key, value);
  }

  clear(): void {
    this.entries.clear();
  }
}
