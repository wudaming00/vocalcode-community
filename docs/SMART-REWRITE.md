# Optional smart rewrite (development / Beta)

This is a manual scratchpad in **History → Rewrite scratchpad**. It is not
automatic cleanup of every dictation and does not replace selected text in
another app. Preview, edit, accept into the scratchpad, then copy explicitly.
An acceptance can be undone while the scratchpad still matches that acceptance.
Editing the source while a request is running invalidates the old candidate.
Accepting or undoing also resets cloud consent; programmatic text changes are
not exempt from the next preview's explicit confirmation.
The **Review in scratchpad** button on an individual history entry loads only
that entry locally, without a clipboard round trip or model request. Replacing
an existing draft requires confirmation. Oversized entries are not silently
truncated; choose a shorter excerpt instead.

## Providers

| Provider | Detection | Generation | Data boundary |
| --- | --- | --- | --- |
| Ollama | Fixed local service at `127.0.0.1:11434`; installed models only | Local CPU, eligible GGUF completion models up to 8B parameters / 8 GiB model file | No remote/cloud models, proxy, redirect or automatic download. The existing local service is trusted. |
| Claude Code CLI | Native executable version and required safety-flag checks | Optional, explicitly selected configured model / Haiku / Sonnet | Separate program and its configured provider. May send text to a cloud service and consume existing subscription/API quota. Fresh consent is required for each preview. |
| Codex CLI | Native executable version only | **Not enabled** | A read-only sandbox still permits reading files. This adapter has not verified a supported completely tool-free text endpoint. Detection is not an authentication or model-success test. |

Click **Detect installed providers** explicitly. Detection does not send your
scratchpad, start a model, install or update a CLI, log in, or download weights.
There is no provider fallback. A missing or incompatible provider gives an
error instead of silently sending text elsewhere.

For Ollama, install/start it and download a suitable model separately. Detection
lists installed candidates, and generation checks model metadata again. The
8 GiB limit is a model-file limit, **not a maximum process RAM guarantee**.
The request uses CPU only, at most four available inference threads, a bounded context/output,
and requests that the model unload after use. Existing Ollama configuration
and concurrently running models can still affect memory and latency.

For Claude Code, use an existing trusted installation and account. On Windows,
the adapter invokes a native executable, not `.cmd` or PowerShell shims. It
safely resolves absolute search directories and excludes the current working
directory even through `..`/directory aliases; direct Windows UNC and device
namespace entries are refused. This is not proof that a mapped drive or a
separately installed executable is trustworthy. It
sends source text via stdin, not shell arguments or a temporary source file.
It uses an isolated working directory, requests no built-in tools, no MCP
servers, no browser integration, no session persistence and one turn, and
validates the returned stream. These are restrictions on a **trusted external
CLI**, not an OS sandbox or a promise that its managed policies, provider or
logs cannot retain data. Changes in the CLI protocol can make the adapter
refuse a candidate until it is reviewed. The CLI's requested spending limit
is not a universal billing guarantee across providers/subscription plans.

No paid account is needed for VocalCode's ordinary local dictation or meetings.

## Limits and safety

- Paste at most **3000 UTF-8 bytes**, not 3000 Chinese characters. The UI shows
  the byte count. Null characters and empty text are rejected.
- Operations: light polish, bullet-list formatting, summary. They return
  candidates, not executed instructions.
- Generation has a 60-second request/process deadline; discovery and bounded
  process cleanup can take a few more seconds. Timeout, shutdown, truncated
  output or incompatible provider responses do not replace the source.
- Local replies must be completed assistant text, with a normal stop reason;
  tool calls, image payloads, provider errors and remote-model markers are
  refused. No returned instruction is executed. This protocol check does not
  establish semantic accuracy or attest the separately trusted local service.
- Discarding a candidate does not recall an already-sent cloud request. It
  prevents acceptance of that reply, while processing may continue until its
  deadline. Nothing retries automatically.
- Requests started while a dictation or meeting is recording/transcribing
  are refused. A later dictation is not blocked; it may still compete for CPU
  with a rewrite that was already running.
- Number/code/negation warnings are conservative heuristics, not a semantic
  guarantee. They may flag harmless formatting such as `1200` → `1,200`, and
  can miss changed meaning in any language. Complete tracked tokens and counts
  are compared in both directions, so `1200` → `12000`, invented numbers and
  removed currency symbols are not accepted as unchanged substring matches.
  English negative contractions are checked as well. Review the complete
  candidate; these checks are not a substitute for understanding its meaning.
- VocalCode does not save this scratchpad or automatically load private
  history into it. Clipboard managers, OS sync, the local model service and
  a separately selected CLI/provider have independent data behavior.
- Local-model verification checks Ollama's model metadata, not its process
  network traffic. An existing service on the fixed loopback endpoint is a
  separately trusted component; VocalCode does not sandbox or attest that
  service. An installed CLI likewise is not proof of working login, quota or
  model compatibility. Those are exercised only by an explicit generation.

## Tested scope (2026-09-23 development build)

Windows WebView2, source/consent/stale-response/undo unit tests, and isolated
synthetic-text requests were exercised. Native macOS CLI behavior is not yet
verified. No installed VocalCode edition or external CLI was upgraded by this
work. See [the product-review results](product-polish-2026-09-23/RESULTS.zh-CN.md)
for final counts, timing and remaining desktop tests.

Provider reference documentation: [Claude CLI](https://code.claude.com/docs/en/cli-reference),
[Codex approvals and security](https://learn.chatgpt.com/docs/agent-approvals-security).
Capabilities are checked against the installed CLI as well as these documents.

The app neither collects CLI credentials nor provides a third-party Claude
login flow. Users sign in separately through the unmodified CLI. Billing and
permitted integration scope remain subject to the provider's current terms;
do not market this as an included or unlimited cloud service.
[Claude integration and authentication guidance](https://code.claude.com/docs/en/legal-and-compliance).
