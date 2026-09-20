// User-facing TypeScript types for tiny-agent (the wrapper API).
// Outcome / event shapes mirror the Rust structs (snake_case, matching Python).

/** Token usage (4 axes). cache_creation is 0 for OpenAI (no creation event). */
export interface Usage {
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
}

export interface ToolCall {
  name: string;
  /** The model's parsed arguments for the call. */
  input: any;
  /** The tool's textual result (what it returned). */
  output: string;
  is_error: boolean;
  /** Audit side-channel the tool may attach (never model-visible content) —
   *  present only when the tool set one. */
  meta?: unknown;
}

export interface RunOutcome {
  final_text: string;
  turns: number;
  stop_reason: string;
  tool_calls: ToolCall[];
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens: number;
  cache_read_input_tokens: number;
  /** Elapsed wall-clock time for the run. */
  elapsed: { secs: number; nanos: number };
}

/** An AgentEvent, externally-tagged (e.g. { TextDelta: "..." }, { Done: { stop_reason } }). */
export type AgentEvent = Record<string, unknown>;

/** A custom tool. `run` may be async; return a string or { content, isError? }. */
export interface Tool {
  name: string;
  description: string;
  /** JSON Schema for the tool's input object. */
  schema: object;
  run: (
    args: any,
  ) =>
    | string
    | { content: string; isError?: boolean }
    | Promise<string | { content: string; isError?: boolean }>;
}

/**
 * The request handed to a custom provider: the conversation + model params. The
 * provider returns the full assistant text for the turn (non-streaming v1).
 */
export interface ProviderRequest {
  system?: string;
  messages: { role: 'user' | 'assistant'; content: unknown[] }[];
  tools: { name: string; description: string; schema: object }[];
  model: string;
  max_tokens: number;
}

/**
 * A custom provider's return: the whole assistant text plus optional
 * `stop_reason` (defaults to `end_turn`) and `usage`. A plain string is
 * shorthand for `{ text: str }`. This is the **non-streaming v1** contract —
 * the full text arrives as one delta; for tool-use, use the built-in provider
 * + custom `Tool`s (a custom provider is text-only).
 */
export interface ProviderResponse {
  text: string;
  stop_reason?: string;
  usage?: Partial<Usage>;
}

/**
 * A custom provider: a function `(request) => ProviderResponse | string`
 * (async allowed), or an object with a `chat(request)` method of the same shape.
 * It replaces the built-in HTTP provider entirely — the agent drives the loop,
 * but each turn's response comes from this callback (no API key, no server).
 */
export type Provider =
  | ((request: ProviderRequest) => ProviderResponse | string | Promise<ProviderResponse | string>)
  | { chat: (request: ProviderRequest) => ProviderResponse | string | Promise<ProviderResponse | string> };

export interface AgentOptions {
  tools?: Tool[];
  provider?: Provider;
  apiKey?: string;
}

export interface ChatOptions {
  onEvent?: (event: AgentEvent) => void;
  tools?: Tool[];
  provider?: Provider;
  apiKey?: string;
}

export declare class CancelToken {
  constructor();
  /** Flip the flag; the running turn stops at the next boundary. */
  cancel(): void;
  isCancelled(): boolean;
}

/** Result of a manual compact(): dropped = messages folded into the summary,
 *  kept = history length after, usage = the summary request's token cost
 *  (null when the provider reported none). */
export interface CompactOutcome {
  dropped: number;
  kept: number;
  usage: Usage | null;
}

export declare class Agent {
  constructor(configPath: string, opts?: AgentOptions);
  /** Append a user message (text) to the conversation. */
  pushUser(text: string): void;
  /** Arm a CancelToken; its flag interrupts the next turn boundary. */
  setCancel(token: CancelToken): void;
  /** Clear the cancel flag — the acceptance-point half of the cancel
   *  contract: call when accepting a NEW task, never during a run (a stop
   *  signalled mid-run must land). Throws while a run is in flight. */
  clearCancel(): void;
  /** Run one turn. `onEvent` (if given) receives each AgentEvent. */
  run(onEvent?: (event: AgentEvent) => void): Promise<RunOutcome>;
  /** Manually fold the history now. `instructions` is an optional focus
   *  hint; resolves with the outcome, or null when nothing was compressible
   *  (history too short); rejects on summary-request failure. The summary's
   *  cost is NOT part of the next run's usage() — meter it from the returned
   *  `usage` yourself. */
  compact(
    instructions?: string | null,
    onEvent?: (event: AgentEvent) => void,
  ): Promise<CompactOutcome | null>;
  /** Cumulative token usage of the current/latest run (4 axes). */
  usage(): Usage;
  /** Conversation history as a list of message objects. */
  messages(): unknown[];
  /** The model name this agent sends with each request. */
  model(): string;
  /** Enable the sandboxed runCode tool for the next request. Requires the
   *  module built with its `code` cargo feature (`npm run build -- --features
   *  code`); otherwise throws with a clear rebuild hint (synchronous method —
   *  no Promise to reject). Throws while a run is in flight. */
  enableCode(): void;
  /** Remove runCode from the model's toolset (next request). Does not
   *  interrupt an in-flight execution. Same feature requirement as
   *  enableCode. */
  disableCode(): void;
  /** Atomically replace the WHOLE code config (switch + every limit / path
   *  knob) from an object shaped like the config file's `"code"` section:
   *  unknown keys reject, missing keys reset to their DEFAULTS (send the
   *  full object to keep a knob). Takes effect for the next execution. */
  setCodeConfig(cfg: object): void;
}

export declare function chat(
  configPath: string,
  message: string,
  opts?: ChatOptions,
): Promise<RunOutcome>;
