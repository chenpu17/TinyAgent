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
  hooks?: Hooks;
  apiKey?: string;
}

/** A model-facing content block a host may push, in either accepted shape:
 *  the Anthropic-style tagged form (`{type: 'image', …}`) or the serde form
 *  `messages()` reports (`{image: {…}}`) — so a transcript block round-trips
 *  through `pushUser(messages()[i].content[j])`. `tool_use` / `tool_result`
 *  are deliberately absent: they are agent-loop internals whose ids must pair
 *  with the model's own output. */
export type UserImage = { media_type: string; data: string };
export type UserBlock =
  | { type: 'text'; text: string }
  | { type: 'image'; media_type: string; data: string }
  | { type: 'document'; media_type: string; data: string }
  | { text: string }
  | { image: UserImage }
  | { document: UserImage };

/** One MCP server, shaped exactly like an entry in the config file's
 *  `"mcp_servers"` array (unknown keys reject; exactly one of
 *  `command` / `url` must be set). */
export interface McpServerConfig {
  name: string;
  /** stdio transport: command to spawn. */
  command?: string;
  /** stdio transport: arguments passed to `command`. */
  args?: string[];
  /** HTTP transports: server URL. */
  url?: string;
  /** HTTP transports: force a transport instead of auto-detection. */
  transport?: 'auto' | 'http' | 'sse';
  /** HTTP transports: extra request headers (e.g. Authorization). */
  headers?: Record<string, string>;
  /** stdio transport: extra env vars for the spawned child. */
  env?: Record<string, string>;
}

export interface ChatOptions {
  onEvent?: (event: AgentEvent) => void;
  tools?: Tool[];
  provider?: Provider;
  hooks?: Hooks;
  apiKey?: string;
}

/** Attribution context handed to the hook methods. `subagent` is null on
 *  top-level calls; a task child's name (or "__code__" for calls originating
 *  inside the runCode sandbox) otherwise. */
export interface HookCtx {
  workdir: string;
  subagent: string | null;
}

/** What beforeTool decides for one pending tool call: null/undefined = run
 *  with the model's original input; a string = refuse (the model sees it as
 *  the error tool_result); { modify } = run with the REPLACED input (the
 *  audit log keeps the original). Anything else is refused (fail-closed). */
export type HookDecision =
  | null
  | undefined
  | string
  | { deny: string }
  | { modify: any };

/** The finished-call view afterTool receives. `executed` is false only when
 *  beforeTool denied the call. */
export interface HookAfterPayload {
  tool: string;
  /** The model's ORIGINAL input (never a modify replacement). */
  input: any;
  output: { content: string; is_error: boolean; meta?: unknown };
  executed: boolean;
  ctx: HookCtx;
}

/** The request projection onChatRequest observes (main-turn requests only;
 *  the compression summary call is not surfaced). */
export interface HookChatRequest {
  system?: string;
  messages: unknown[];
  tools: { name: string; description: string; schema: object }[];
  model: string;
  max_tokens: number;
  cache: boolean;
}

/**
 * The interception layer, as a duck-typed object — provide any subset of the
 * methods; missing ones are no-ops. `beforeTool` gates a call BEFORE it runs
 * (return null / deny string / { modify: input }); a throwing or ill-formed
 * beforeTool FAILS CLOSED (the call is denied, the reason rides the message).
 * `afterTool` and `onChatRequest` are observe-only; a throw in them is
 * swallowed (logged to stderr). Task sub-agents inherit the layer; attribute
 * calls via ctx.subagent. NOTE: an agent rebuilt from a resumed session does
 * NOT carry hooks over — reinstall with setHooks.
 */
export interface Hooks {
  beforeTool?(
    tool: string,
    input: any,
    ctx: HookCtx,
  ): HookDecision | Promise<HookDecision>;
  afterTool?(payload: HookAfterPayload): void | Promise<void>;
  onChatRequest?(request: HookChatRequest): void | Promise<void>;
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

/** One tool's estimated schema cost (name + description + schema JSON,
 *  chars/4 — an estimate, never a provider measurement). */
export interface ToolEntry {
  name: string;
  tokens: number;
}

/** One origin bucket of tool-schema cost. `tools` lists every tool in
 *  tool-list order (unsorted). */
export interface ToolCategory {
  tokens: number;
  tools: ToolEntry[];
}

/** The system prompt split into its three sources (separator bytes belong
 *  to the adjacent segment; the three sum to `tokens`). */
export interface SystemBreakdown {
  env: number;
  base: number;
  skill_intro: number;
  tokens: number;
}

/** Message history aggregated by block kind (snake_case keys mirror the
 *  Rust serde shape). There is no `thinking` kind: thinking never enters
 *  history. */
export interface MessageKinds {
  user_text: number;
  assistant_text: number;
  tool_use: number;
  tool_result: number;
  image: number;
  document: number;
  skill_content: number;
  other: number;
}

/** Calibration info for a scaled breakdown: `scale` is the anchored
 *  request's measured/estimated ratio; every component was multiplied by
 *  it (a heuristic — see the Rust `context_report` module docs). */
export interface Calibration {
  measured_prompt_tokens: number;
  scale: number;
}

/** Estimated token composition of the next request's context. EVERY number
 *  is an estimate (chars/4 yardstick); `usage()` stays the billing
 *  authority. `calibration` is null until a measured anchor exists (and
 *  after it's invalidated by a fold / tool-table change). */
export interface ContextBreakdown {
  system: SystemBreakdown;
  tools: {
    builtin: ToolCategory;
    mcp: ToolCategory;
    skill: ToolCategory;
    custom: ToolCategory;
    tokens: number;
  };
  messages: { tokens: number; by_kind: MessageKinds };
  total_tokens: number;
  calibration: Calibration | null;
}

export declare class Agent {
  constructor(configPath: string, opts?: AgentOptions);
  /** Async constructor — same result as `new Agent(...)`, but config load and
   *  tool assembly (spawning + handshaking every configured MCP server, the
   *  seconds-long part with a slow server) run on a worker thread, so the JS
   *  event loop keeps ticking. Malformed `opts.tools` / `opts.provider` /
   *  `opts.hooks` throw SYNCHRONOUSLY at call time (the returned Promise is
   *  not involved); config / assembly errors reject the Promise. Resolves to
   *  an instance of this wrapper class (not the raw native one), so `run` /
   *  `compact` keep their onEvent guard. */
  static create(configPath: string, opts?: AgentOptions): Promise<Agent>;
  /** Append a user message to the conversation. A string is one text block;
   *  an object is one block; an array is one block per element (in order).
   *  `tool_use` / `tool_result` blocks are rejected (agent-loop internals). */
  pushUser(content: string | UserBlock | Array<string | UserBlock>): void;
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
  /** Estimated token composition of the next request's context (see
   *  ContextBreakdown — every number is an estimate). Read-only; busy-errors
   *  while a run is in flight, like usage(). */
  contextBreakdown(): ContextBreakdown;
  /** Conversation history as a list of message objects. */
  messages(): unknown[];
  /** The model name this agent sends with each request. */
  model(): string;
  /** Swap the model used from the next request on (between runs). Does NOT
   *  propagate to `task` sub-agents constructed before the swap (kernel
   *  scope decision: a sub-agent snapshots its config). */
  setModel(model: string): void;
  /** Set (or clear, with null) the reasoning effort passed through verbatim
   *  for the next request. */
  setReasoningEffort(effort: string | null): void;
  /** Change the per-request max_tokens. Accepts an integer in
   *  0..=4294967295; values below 1 are clamped to 1. Fractional, negative,
   *  or out-of-range values throw (they are not silently truncated/wrapped). */
  setMaxTokens(maxTokens: number): void;
  /** Change the context budget used by compression; 0 falls back to the
   *  char heuristic. Negative, fractional, or out-of-range values throw. */
  setMaxContextTokens(tokens: number): void;
  /** Spawn + handshake one MCP server and register its tools for the next
   *  request; RETURNS the exposed tool names (synchronous — the handshake
   *  blocks the JS thread). A name collision is a recoverable error (nothing
   *  is registered). Throws while a run is in flight. */
  addMcpServer(cfg: McpServerConfig): string[];
  /** Tear down one MCP server by name; returns how many tools were removed.
   *  Unknown names throw. Throws while a run is in flight. */
  removeMcpServer(name: string): number;
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
  /** Install (or replace) the interception layer — see [[Hooks]]. Methods
   *  are resolved at call time; a present-but-not-a-function property
   *  throws here, not mid-run. Throws while a run is in flight. */
  setHooks(hooks: Hooks): void;
}

export declare function chat(
  configPath: string,
  message: string,
  opts?: ChatOptions,
): Promise<RunOutcome>;
