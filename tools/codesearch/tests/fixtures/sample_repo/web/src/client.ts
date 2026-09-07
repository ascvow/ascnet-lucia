import { loadSettings, type Settings } from "./store";

/** 单次请求允许的最大退避毫秒数。 */
export const MAX_BACKOFF_MS = 30_000;

/** 请求失败时抛出的错误。 */
export class RequestFailedError extends Error {
  constructor(
    readonly attempts: number,
    readonly lastStatus: number,
  ) {
    super(`request failed after ${attempts} attempts (status ${lastStatus})`);
  }
}

/**
 * 带重试的 HTTP 客户端。
 *
 * 请求失败后按指数退避重试，直到成功或耗尽尝试次数。
 */
export class HttpClient {
  private readonly settings: Settings;

  constructor(settings: Settings = loadSettings()) {
    this.settings = settings;
  }

  /** 发送请求，失败后按退避策略重试。 */
  async fetchWithRetry(path: string): Promise<Response> {
    let lastStatus = 0;
    for (let attempt = 1; attempt <= this.settings.maxAttempts; attempt += 1) {
      const response = await fetch(`${this.settings.endpoint}${path}`);
      if (response.ok) {
        return response;
      }
      lastStatus = response.status;
      if (attempt < this.settings.maxAttempts) {
        await delay(this.backoffFor(attempt));
      }
    }
    throw new RequestFailedError(this.settings.maxAttempts, lastStatus);
  }

  /** 计算第 attempt 次失败后的退避时长。 */
  backoffFor(attempt: number): number {
    return Math.min(this.settings.baseDelayMs * 2 ** (attempt - 1), MAX_BACKOFF_MS);
  }
}

const delay = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));
