/**
 * Backend streaming phase consumed by terminal presentation adapters.
 */
export type SpinnerMode =
  | "responding"
  | "thinking"
  | "tool-use"
  | "tool-input"
  | "requesting";
