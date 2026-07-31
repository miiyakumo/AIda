# Workflow Examples

> rust-router 的工作流程示例

## Example 1: Error Code with Project Context

```
User: "Why am I getting E0382 in my trading system?"

Analysis:
1. Entry: Layer 1 (E0382 = ownership/move error)
2. Load: m01-ownership skill
3. Context: "trading system" → preserve business constraints

Trace UP ↑:
- E0382 in trading context
- Check project requirements: "immutable audit records"
- Finding: Trading data should be shared, not moved

Response:
"E0382 indicates a value was moved when still needed.
In a trading system, transaction records
should be immutable and shareable for audit purposes.

Instead of cloning, consider:
- Arc<TradeRecord> for shared immutable access
- This aligns with financial audit requirements

See: m01-ownership and the project's audit requirements"
```

## Example 2: Design Question

```
User: "How should I handle user authentication?"

1. Entry: Layer 2 (design question)
2. Inspect project security and HTTP constraints
3. Load: m09-domain for authentication invariants
4. Trace DOWN: m06-error-handling, m07-concurrency
5. Answer using the project's chosen authentication mechanism
```

## Example 3: Comparative Query

```
User: "Compare tokio and async-std"

1. Detect: "compare" → Enable negotiation
2. Load both runtime knowledge sources
3. Assess confidence for each
4. Synthesize with disclosed gaps
5. Answer: Structured comparison table
```

## Example 4: Multi-Layer Trace

```
User: "My web API reports Rc cannot be sent between threads"

1. Entry: Layer 1 (Send/Sync error)
2. Load: m07-concurrency
3. Inspect the project's web state management pattern
4. Load m07 to explain Send/Sync bounds
5. Answer: Use Arc instead of Rc, or move to thread-local
```

## Example 5: Intent Analysis Request

```
User: "Analyze this question: How do I share state in actix-web?"

Analysis Steps:
1. Extract Keywords: share, state, actix-web
2. Identify Entry Layer: Layer 1 (sharing = concurrency)
3. Map to Skill: m07-concurrency
4. Report:
   - Layer 1: Concurrency (state sharing mechanisms)
   - Project context: Actix handler and application-state patterns
   - Suggested trace: language mechanics → project constraints
5. Invoke: m07-concurrency, then inspect the relevant project code
```
