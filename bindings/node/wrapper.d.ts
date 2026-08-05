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

export declare class Agent {
  constructor(configPath: string, opts?: AgentOptions);
  /** Append a user message (text) to the conversation. */
  pushUser(text: string): void;
  /** Arm a CancelToken; its flag interrupts the next turn boundary. */
  setCancel(token: CancelToken): void;
  /** Run one turn. `onEvent` (if given) receives each AgentEvent. */
  run(onEvent?: (event: AgentEvent) => void): Promise<RunOutcome>;
  /** Cumulative token usage of the current/latest run (4 axes). */
  usage(): Usage;
  /** Conversation history as a list of message objects. */
  messages(): unknown[];
  /** The model name this agent sends with each request. */
  model(): string;
}

export declare function chat(
  configPath: string,
  message: string,
  opts?: ChatOptions,
): Promise<RunOutcome>;
