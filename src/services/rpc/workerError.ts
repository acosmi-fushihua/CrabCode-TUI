/** Worker-private JSON-RPC error carrying a stable code and redacted data. */
export class WorkerError extends Error {
  constructor(
    public code: number,
    message: string,
    public data?: unknown,
  ) {
    super(message);
    this.name = "WorkerError";
  }
}
