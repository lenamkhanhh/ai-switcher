# Codex Optimization Timeline

> Nguồn sự thật duy nhất về cấu hình Codex trên máy này. Đọc trước khi thay đổi, nâng cấp, benchmark hoặc rollback Codex.

**Cập nhật:** 2026-06-19  
**Môi trường:** Windows, PowerShell  
**Codex CLI đã kiểm tra:** `0.141.0`  
**Cấu hình chính:** `C:\Users\HP\.codex\config.toml`

## Trạng thái hiện tại

- Default: `gpt-5.5`, reasoning `medium`, service tier `default`.
- `tool_output_token_limit = 4000`.
- Multi-agent, goals, memories, JS REPL và toàn bộ MCP tắt mặc định.
- Browser/Documents/PDF/Spreadsheets/Presentations tắt mặc định; profile chuyên dụng bật lại khi cần.
- Dùng `codex.cmd` trên PowerShell để tránh Execution Policy chặn `codex.ps1`.
- Sửa config/tool schema xong phải mở thread mới hoặc restart Codex.

## Timeline

### 2026-06-19 — Baseline và research

Log Codex local được đọc read-only. Kết quả cho thấy usage limit không tỷ lệ trực tiếp với output; nhiều model-tool-model round trip gửi lại context lớn và tiêu tốn đáng kể.

Paper, GitHub và tài liệu chính thức dẫn đến các quyết định:

1. Tắt MCP/plugin không dùng để giảm static tool schema.
2. Tắt multi-agent mặc định; chỉ bật cho nhánh độc lập.
3. Hạ tool output cap từ 8000 xuống 4000.
4. Batch read/search/test độc lập trong một tool round.
5. Chỉ nén log, trace, JSON và tài liệu; không token-prune code/patch/error cần chính xác.
6. Compact tại milestone, không compact giữa debug chain.

Lưu ý quan trọng từ [OpenAI Prompt Caching](https://developers.openai.com/api/docs/guides/prompt-caching): cache cần exact prefix và cached tokens vẫn tính vào rate limit. Cache hit cao không có nghĩa limit giảm tương ứng.

Nguồn nghiên cứu:

- [Codex best practices](https://developers.openai.com/codex/learn/best-practices)
- [Codex MCP](https://developers.openai.com/codex/mcp)
- [Codex subagents](https://developers.openai.com/codex/concepts/subagents)
- [ACON](https://arxiv.org/abs/2510.00615) / [GitHub](https://github.com/microsoft/acon)
- [LongLLMLingua](https://aclanthology.org/2024.acl-long.91/) / [GitHub](https://github.com/microsoft/LLMLingua)
- [Selective Context](https://arxiv.org/abs/2310.06201) / [GitHub](https://github.com/liyucheng09/Selective_Context)
- [SWE-agent](https://arxiv.org/abs/2405.15793) / [GitHub](https://github.com/SWE-agent/SWE-agent)

### 2026-06-19 — Cấu hình lean được áp dụng

| Cấu hình | Trước | Hiện tại |
| --- | --- | --- |
| `tool_output_token_limit` | `8000` | `4000` |
| `features.multi_agent` | `true` | `false` |
| Headroom | enabled | disabled |
| Context7 | enabled | disabled |
| OpenAI Docs MCP | enabled | disabled |
| Node REPL | disabled | disabled |

Snapshot:

```toml
model = "gpt-5.5"
model_reasoning_effort = "medium"
tool_output_token_limit = 4000
service_tier = "default"

[features]
multi_agent = false
js_repl = false
goals = false
memories = false

[mcp_servers.headroom]
enabled = false
[mcp_servers.context7]
enabled = false
[mcp_servers.openaiDeveloperDocs]
enabled = false
[mcp_servers.node_repl]
enabled = false
```

### 2026-06-19 — A/B test

Cùng prompt đọc `package.json`, GPT-5.5 medium, read-only, hai model turns:

| Chỉ số | Baseline warm | Optimized warm | Thay đổi |
| --- | ---: | ---: | ---: |
| First-prompt input | 15,826 | 13,394 | -15.4% |
| Tổng raw input | 32,043 | 27,202 | -15.1% |
| Uncached input | 5,419 | 1,090 | -79.9% |
| CLI `tokens used` | 5,511 | 1,205 | -78.1% |

Cold-cache đầu tiên sau đổi schema dùng 14,269 token. Kết luận an toàn: raw context giảm khoảng 15%, không phải usage limit giảm 78%.

`codex.cmd doctor`: `17 ok`, `1 idle`, `0 warn`, `0 fail`; config parse OK, databases healthy, bốn MCP disabled.

### 2026-06-19 — Recheck token/limit sau tối ưu

Chạy lại cùng loại tác vụ read-only trên `package.json` bằng `gpt-5.5` medium, sandbox read-only, session ephemeral. Ba lượt hợp lệ đều trả đúng package `ai-account-switcher` phiên bản `0.5.5` và bốn script; không sửa file. Một lượt warm xác nhận cuối có số liệu:

| Chỉ số | Optimized warm cũ | Recheck warm | Thay đổi |
| --- | ---: | ---: | ---: |
| Tổng raw input | 27,202 | 27,244 | +0.15% |
| Cached input | 26,112* | 26,112 | 0% |
| Uncached input | 1,090 | 1,132 | +3.9% |
| Output | 115* | 107 | -7.0% |
| Billable-style (`uncached + output`) | 1,205 | 1,239 | +2.8% |

`*` Suy ra từ số liệu cũ `raw - uncached` và `tokens used - uncached` để so sánh cùng cách tính.

Kết luận: cấu hình tối ưu vẫn còn hiệu lực và kết quả nằm trong nhiễu nhỏ; chưa có bằng chứng token/usage limit được cải thiện thêm so với A/B optimized trước đó. So với baseline warm ban đầu, raw context vẫn giảm khoảng 15%. Cached tokens vẫn tính vào rate limit, nên mức tiêu thụ limit gần với raw input hơn là chỉ số billable-style.

Kiểm tra môi trường: Codex CLI `0.141.0`; `tool_output_token_limit = 4000`; multi-agent, goals, memories, JS REPL và cả bốn MCP đều disabled; `codex.cmd doctor` trả `17 ok`, `1 idle`, `0 warn`, `0 fail`.

### 2026-06-19 — Tối ưu thêm profile `quick`

Theo Codex manual hiện hành, reasoning cao dùng thêm token/rate limit, web search có thể tắt bằng `web_search = "disabled"`, và skill/MCP thêm context/tool schema. Không cài thêm MCP nén vì task local nhỏ không hưởng lợi đủ để bù static schema.

Thay đổi duy nhất trong `C:\Users\HP\.codex\quick.config.toml`:

```toml
web_search = "disabled" # trước: "cached"
```

A/B cùng prompt read-only trên `package.json`, session ephemeral, ba lượt mỗi cấu hình:

| Cấu hình | Raw input trung bình | So với default | Kết quả |
| --- | ---: | ---: | --- |
| Default `gpt-5.5` medium | 27,244 | baseline | 3/3 đúng |
| `quick` trước, web cached | 22,649 | -16.9% | 3/3 đúng |
| `quick` sau, web disabled | 17,270 | -36.6% | 3/3 đúng |

Riêng việc tắt web search giảm raw input của profile `quick` thêm 23.8%. Post-change `--profile quick --strict-config` pass 3/3, không có command lỗi và không sửa source. Cached input chỉ 8,192 nên uncached token của cache mới chưa giảm; kết luận tập trung vào raw input/rate-limit footprint. `doctor` không hỗ trợ cờ `--profile`, nhưng config profile đã được parser runtime xác nhận bằng `--strict-config`.

Dùng `codex.cmd --profile quick` cho task local nhỏ/rõ. Khi cần web, dùng profile `research` hoặc override một phiên bằng `-c 'web_search="cached"'`. Default `gpt-5.5` medium không đổi để tránh giảm chất lượng ở task khó.

### 2026-06-19 — Đo limit thực và thêm profile `limit`

Snapshot rollout local cho tài khoản Plus cho thấy token count không dự đoán trực tiếp mức tụt usage. Ba task `package.json` bằng default GPT-5.5 làm cửa sổ 5 giờ tăng lần lượt khoảng `33% → 38% → 41%` (3–5 điểm/task). Sau chuỗi benchmark dài, cửa sổ đã ở 94%; ba task tương đương bằng `quick` GPT-5.4 mini chỉ tăng `94% → 94% → 95%`, còn cửa sổ tuần giữ nguyên 15%. Kết quả cả 3/3 đúng.

Kết luận: GPT-5.4 mini low là tối ưu limit có tác động lớn nhất cho routine local messages; điều này khớp Codex Pricing hiện hành. Prompt cache cao không đảm bảo usage limit giảm tương ứng.

Tạo `C:\Users\HP\.codex\limit.config.toml` làm chế độ cực lean, không thay default hoặc `quick`:

- GPT-5.4 mini, reasoning low, standard tier.
- Web, MCP/multi-agent và toàn bộ skill discovery tắt.
- `tool_output_token_limit = 2000`.
- `project_doc_max_bytes = 8192`, vẫn đủ chứa global + project `AGENTS.md` hiện tại (4,587 byte).

Offline `codex debug prompt-input` A/B (không tiêu usage):

| Profile | Prompt JSON chars | Skill path mentions | Thay đổi |
| --- | ---: | ---: | ---: |
| `quick` | 20,792 | 26 | baseline |
| `limit` | 10,589 | 0 | -49.1% |

Parser trả exit 0 cho cả hai profile. Chưa chạy live eval cho `limit` vì cửa sổ 5 giờ còn 5%, reset lúc 2026-06-19 23:36 ICT; tránh benchmark làm khóa phiên. Sau reset cần chạy 3 task capability/regression trước khi dùng cho thay đổi nhạy cảm. Profile này chỉ dành cho local coding nhỏ/rõ; không dùng khi cần web, docs mới, security/payment, UI/browser, hoặc workflow cần skill chuyên dụng.

### 2026-06-20 — Profile `limit` thêm một nấc

Thử `model_reasoning_effort = 'minimal'` trên GPT-5.4 mini: parser chấp nhận nhưng endpoint kết thúc cả 3/3 lượt với exit 1, không có agent answer; cửa sổ 5 giờ tăng khoảng `9% → 10%`. Không áp dụng `minimal`, tiếp tục dùng `low`.

Thay đổi được giữ trong `C:\Users\HP\.codex\limit.config.toml`:

```toml
model_reasoning_effort = 'low'
model_verbosity = 'low'
model_reasoning_summary = 'none'
tool_output_token_limit = 1000 # trước: 2000
```

Live eval read-only trên `package.json`, GPT-5.4 mini low, web/skills disabled, ba session ephemeral:

| Lượt | Raw input | Cached | Output | Reasoning | Kết quả |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 | 12,768 | 6,144 | 140 | 20 | PASS |
| 2 | 12,767 | 6,144 | 138 | 17 | PASS |
| 3 | 12,763 | 6,144 | 131 | 17 | PASS |

Trung bình raw input `12,766`, giảm 26.1% so với `quick` (`17,270`) và 53.1% so với default (`27,244`); correctness 3/3, không có command lỗi. `model_verbosity = 'low'` và cap 1000 chủ yếu giúp task có output/log dài; mức giảm raw lớn ở phép đo này còn đến từ toàn bộ cấu hình `limit` (đặc biệt tắt skill discovery và web), không quy riêng cho hai knob mới.

Codex CLI vẫn phát warning về ba custom agent role thiếu `name` và một số plugin manifest/icon bị ignore. Chúng không làm smoke test sai nhưng nên sửa riêng nếu muốn giảm startup noise; chưa đụng vào vì ngoài phạm vi token profile và có thể ảnh hưởng cấu hình agent/plugin dùng chung.

### 2026-06-20 — Thử `context-mode` và RTK cho Codex Windows

Nghiên cứu nguồn chính:

- `context-mode` README/platform support khẳng định có support `codex` hooks và MCP tools.
- `RTK` README hiện tại ghi rõ Codex integration là `AGENTS.md + RTK.md instructions`, không phải hard hook; trên Windows RTK thiếu auto-rewrite hook và khuyến nghị WSL cho full support.

Links:

- [context-mode README](https://github.com/mksglu/context-mode)
- [context-mode platform support](https://github.com/mksglu/context-mode/blob/main/docs/platform-support.md)
- [RTK README](https://github.com/rtk-ai/rtk)
- [RTK Windows limitation discussion](https://github.com/rtk-ai/rtk/discussions/671)

Quyết định test:

1. Không cài RTK cho Codex Windows ở profile thật vì cơ chế hiện tại chủ yếu dựa vào model tự nhớ prefix, không có enforced rewrite như Unix/WSL.
2. Cài tạm `context-mode@1.0.162`, tạo profile thử nghiệm `ctxlimit`, hook config global, rồi benchmark live bằng `codex.cmd exec --json --ephemeral`.

Kết quả xác minh:

- `context-mode.cmd doctor` pass storage/server/FTS5; Node `26.3.0`, Python `3.14.1`, bash từ Git for Windows có sẵn.
- Trong profile thử nghiệm, agent nhìn thấy các tool: `ctx_doctor`, `ctx_execute`, `ctx_execute_file`, `ctx_fetch_and_index`, `ctx_index`, `ctx_insight`, `ctx_purge`, `ctx_search`, `ctx_stats`, `ctx_upgrade`.
- Tuy vậy trên Codex CLI Windows non-interactive (`exec`) agent không tự route ổn sang `ctx_*`; khi ép dùng `ctx_execute`/`ctx_batch_execute` thì MCP tool call bị trả `user cancelled MCP tool call`, rồi agent fallback về shell thường.

Benchmark live:

| Task | Profile | Raw input | Cached | Output | Reasoning | Command/MCP failures | Ghi chú |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| Đọc `package.json` | `limit` | 13,055 | 7,680 | 136 | 17 | 0 | PASS |
| Đọc `package.json` | `ctxlimit` | 24,661 | 11,776 | 135 | 21 | 0 | PASS nhưng raw cao hơn 88.9% |
| Phân tích 200 commit gần nhất | `limit` | 31,389 | 24,064 | 1,033 | 144 | 0 | PASS |
| Phân tích 200 commit gần nhất | `ctxlimit` (auto route) | 38,870 | 25,600 | 890 | 104 | 1 | PASS nhưng raw cao hơn 23.8% |
| Phân tích 200 commit gần nhất | `ctxlimit` (ép dùng `ctx_*`) | 127,019 | 110,592 | 2,877 | 537 | MCP `ctx_execute` + `ctx_batch_execute` fail | Không dùng được |

Kết luận:

- Với Codex CLI `0.141.0` trên Windows/PowerShell, `context-mode` không cải thiện token/limit trong workflow local hiện tại; profile thử nghiệm còn tệ hơn `limit`.
- Overhead của hooks/MCP/tool schema lớn hơn lợi ích thực tế trong các bài test này.
- Khi ép dùng `ctx_*`, tool calls không ổn định trong non-interactive `exec`, làm usage tăng mạnh do retry/fallback.
- Vì vậy đã rollback toàn bộ thử nghiệm: xóa profile `ctxlimit`, xóa `hooks.json`, gỡ `context-mode` global package, và không giữ block hướng dẫn context-mode trong repo `AGENTS.md`.
- Trạng thái khuyến nghị vẫn là giữ `limit`/`quick`; nếu sau này muốn thử lại RTK thì chỉ đáng làm trong WSL hoặc khi Codex có hard-hook/native rewrite đáng tin cậy hơn trên Windows.

### 2026-06-20 — Thử hướng “profile split + prompt pattern” thêm một vòng

Mục tiêu: xem còn tối ưu thêm được bằng cách tách profile hẹp theo loại task và viết prompt ngắn/gọn hơn hay không, không cài thêm MCP/plugin.

Đã tạo tạm hai profile rồi benchmark live bằng `codex.cmd exec --json --ephemeral`:

- `micro`: GPT-5.4 mini low, `tool_output_token_limit = 600`, `project_doc_max_bytes = 6144`, web off, skills off.
- `scan`: GPT-5.4 mini low, `sandbox_mode = "read-only"`, `tool_output_token_limit = 1200`, `project_doc_max_bytes = 6144`, web off, skills off.

Kết quả:

| Task | Profile / prompt | Raw input | Cached | Output | Reasoning | Failures | Kết luận |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| Đọc `package.json` | `limit` + prompt rõ scope | 12,762 | 11,776 | 131 | 16 | 0 | baseline tốt |
| Đọc `package.json` | `micro` + cùng prompt | 12,772 | 6,144 | 140 | 28 | 0 | tệ hơn nhẹ |
| Đọc `package.json` | `micro` + prompt cực ngắn | 12,761 | 6,144 | 137 | 19 | 0 | gần như không hơn |
| Đọc `package.json` | `micro` warm lần 2 | 12,764 | 6,144 | 133 | 18 | 0 | vẫn không hơn `limit` |
| Phân tích 200 commit | `limit` + prompt rõ scope | 12,844 | 7,680 | 573 | 120 | 0 | tốt nhất vòng này |
| Phân tích 200 commit | `scan` | 40,491 | 36,352 | 1,078 | 185 | 2 | tệ hơn nhiều |
| Phân tích 200 commit | `scan` warm lần 2 | 31,943 | 28,672 | 1,142 | 223 | 1 | vẫn tệ hơn |
| Phân tích 200 commit | `limit` + prompt quá ngắn | 47,424 | 43,520 | 1,987 | 228 | 1 | tệ nhất trong các biến thể không-MCP |

Kết luận:

- Tách thêm profile `micro`/`scan` không cho lợi ích thực tế trên Codex CLI Windows hiện tại; đã rollback cả hai profile.
- Prompt “quá ngắn” hoặc mơ hồ có thể làm usage tăng mạnh do agent tự đoán cách làm, retry nhiều hơn và trả lời dài hơn.
- Điều có ích hơn là prompt ngắn-vừa nhưng phải rõ scope, rõ output, rõ ràng “read-only / no file changes”.
- Khuyến nghị cập nhật: với task local nhỏ hoặc read-only, tiếp tục dùng `limit`; với task local thường ngày, dùng `quick`; không thêm profile mới chỉ để “lean hơn một chút” nếu chưa có benchmark chứng minh.

Prompt pattern khuyến nghị:

```text
Read-only.
Scope: <1 file / 1 thư mục / 1 khoảng log / 1 số commit>.
Need: <đúng thứ cần trả lời>.
Output: <1 câu / list ngắn / không thêm giải thích>.
No file changes.
```

Ví dụ tốt:

```text
Read-only package.json. Report exactly its name, version, and npm script names in one concise sentence. Do not modify files.
```

Ví dụ xấu:

```text
package.json only: name, version, script keys. one concise sentence. no file changes.
```

Lý do: bản “xấu” ngắn hơn nhưng mơ hồ hơn, nên trong benchmark thực tế không tiết kiệm được và có lúc còn tăng token/failed attempts.

### 2026-06-19 — Hợp nhất tài liệu

Ba file Codex ở `web-luyen-code` được hợp nhất vào timeline này rồi xóa khỏi repo cũ. `ai-switcher/AGENTS.md` được thêm chỉ dẫn để chat sau tự đọc file này.

## Hướng dẫn sử dụng

### Chọn profile

```powershell
codex.cmd --profile quick     # task nhỏ/rõ; GPT-5.4 mini low
codex.cmd --profile limit     # task local rất nhỏ; cực lean, không web/skills
codex.cmd                     # task thường; GPT-5.5 medium
codex.cmd --profile deep      # debug/refactor/kiến trúc; GPT-5.5 high
codex.cmd --profile review    # review read-only
codex.cmd --profile ml        # RL/ML, giữ seed/data/hyperparameter
codex.cmd --profile strict    # read-only, web cache
```

Các profile khác: `research`, `ui`, `docs`, `pdf`, `sheets`, `slides` tại `C:\Users\HP\.codex\<profile>.config.toml`.

### Bật tool theo task

```powershell
# Library/framework docs
codex.cmd --profile research -c mcp_servers.context7.enabled=true

# Codex/OpenAI docs
codex.cmd --profile research -c mcp_servers.openaiDeveloperDocs.enabled=true

# Log/JSON/trace lớn
codex.cmd -c mcp_servers.headroom.enabled=true

# UI cần Browser Plugin + Node REPL
codex.cmd --profile ui -c mcp_servers.node_repl.enabled=true

# Multi-agent cho nhánh độc lập
codex.cmd --enable multi_agent
```

Codex `0.141.0` chưa overlay `mcp_servers` ổn định từ profile file trong kiểm tra local, nên dùng CLI override `-c`.

### Prompt mẫu

```text
Goal: <kết quả cụ thể>.
Context: <file/thư mục/symbol/log liên quan>.
Constraints: <API, seed, config hoặc hành vi phải giữ>.
Done when: <test/command/tiêu chí hoàn thành>.

Batch các thao tác đọc/search độc lập trong một tool round, sửa đúng phạm vi,
chạy verification hẹp nhất và không in raw log dài.
```

### Quy tắc vận hành

1. Mục tiêu mới thì mở thread mới hoặc `/clear`.
2. Task rõ dùng `quick`; chỉ dùng `deep` khi thực sự cần.
3. Muốn giữ limit thì `/fast off`.
4. Đưa path log thay vì paste toàn bộ; giới hạn khoảng 80 dòng liên quan.
5. Multi-agent chỉ cho nhánh độc lập.
6. `/compact` sau exploration/milestone/trước review, không giữa debug chain.
7. `/status` xem usage; `/context` xem phần chiếm context.

Checkpoint khi compact:

```text
Goal:
Confirmed facts:
Decisions:
Files/symbols involved:
Failed attempts:
Constraints that must survive:
Next action:
Verification remaining:
```

## Khi Codex update

1. Đọc file này và `AGENTS.md`.
2. Chạy `codex.cmd --version`, `codex.cmd doctor`, `codex.cmd mcp list`.
3. Kiểm tra `tool_output_token_limit`, `features.multi_agent`, `mcp_servers.*.enabled` và profile V2 còn hợp lệ.
4. Không tự bật toàn bộ MCP/plugin.
5. Chạy lại A/B cùng prompt/model/sandbox, ít nhất ba task tương đương.
6. Ghi thêm mục có ngày vào Timeline; không xóa số liệu lịch sử.

## Rollback

Ưu tiên bật riêng cho một phiên bằng các lệnh ở trên. Chỉ rollback toàn bộ khi có bằng chứng workflow/chất lượng bị ảnh hưởng:

```toml
tool_output_token_limit = 8000
[features]
multi_agent = true
[mcp_servers.headroom]
enabled = true
[mcp_servers.context7]
enabled = true
[mcp_servers.openaiDeveloperDocs]
enabled = true
```

Sau thay đổi hoặc rollback: mở thread mới/restart Codex và ghi sự kiện vào Timeline.

### 2026-06-24 - VS Code account switching

- Confirmed the Codex IDE extension reads `~/.codex/auth.json`, while profile launchers use
  `CODEX_HOME` and previously changed only the CLI account.
- App-server probe: an existing process kept the old account after `auth.json` changed; a fresh
  process immediately loaded the selected account. Therefore VS Code requires
  `Developer: Reload Window` after switching.
- Added reversible credential sync with a machine-default backup, outgoing refresh-token
  preservation, startup self-healing, and three focused Rust tests.

### 2026-07-10 - Windows Codex auto-switch runtime refresh

- Fixed the 100% cutoff path: the selected account is now resolved from the active profile marker,
  so an account already marked `Exhausted` still triggers fallback.
- Auto-switch now applies the same Codex IDE credential sync and narrow `codex app-server` restart
  used by manual switching. VS Code stays open and reconnects on the next Codex thread/request.
- Fixed fallback to Machine default by clearing the active profile marker instead of writing a
  synthetic default profile path.
- Verification: `cargo test --lib` passed 69/69, `npm.cmd run build` passed, and
  `npm.cmd audit --omit=dev --audit-level=high` reported 0 vulnerabilities.

### 2026-07-10 - Windows release runs without a console window

- Confirmed the installed executable was built as `IMAGE_SUBSYSTEM_WINDOWS_CUI`, which caused a
  terminal window to stay behind the Tauri UI and tied normal use to that console window.
- Added the release-only Windows GUI subsystem attribute in `src-tauri/src/main.rs`; debug builds
  keep their console for diagnostics.

### 2026-07-10 - Hidden Codex login prompt

- Disabled the user-level `Roblox_Studio` MCP in `C:\Users\HP\.codex\config.toml` after it was
  found spawning `cmd.exe /c ...\Roblox\mcp.bat` and `StudioMCP.exe` windows behind Codex.
- Changed Windows Codex account login to run `codex login --device-auth` without opening a
  terminal window. The backend captures only the device URL and one-time code and emits them to the
  app UI; tokens stay in Codex-managed `auth.json`.
- Verification: `cargo test --lib` passed 71/71 and `npm.cmd run build` passed.

### 2026-07-10 - Removed quota curl process spawning

- Follow-up validation found quota refresh still spawned short-lived `curl.exe` processes. Even
  when hidden, this was risky because bearer tokens were visible in the child process command line.
- Replaced the shared quota/pricing HTTP helper internals with `reqwest::blocking::Client`, keeping
  the old Rust function names for compatibility but removing the external `curl` process entirely.
- Added a regression test to ensure invalid HTTP header errors mention only the header name and do
  not echo secret values.
- Installed the rebuilt release exe to `C:\Users\HP\AppData\Local\AI Account Switcher`.
- Verification: `cargo test --lib` passed 72/72, `npm.cmd run build` passed, `npm.cmd run tauri
  build` passed, and a descendant-process watcher after focusing the app showed only
  `msedgewebview2.exe` children (no `cmd.exe`, `WindowsTerminal.exe`, `OpenConsole.exe`, or
  `curl.exe`).

### 2026-07-11 - Codex auto-switch handoff and Windows Auto Session tab

- Added a best-effort Codex handoff prompt after subscription auto-switch: the app reads the latest
  Codex rollout conversation, skips tool/function outputs, redacts secret-like lines, writes a
  handoff markdown file, and emits it to the UI for clipboard copy.
- Re-enabled the Auto Session tab for the Windows Codex-only build so per-account scheduled prime
  times can be configured directly on Windows.
- Verification: `cargo test --lib` passed 74/74, `npm.cmd run build` passed, `npm.cmd run tauri
  build` passed, installed exe hash matched the release exe, and the descendant-process watcher
  still showed only `msedgewebview2.exe` children.

### 2026-07-11 - Fast Codex quota-error watcher

- Added a 10-second Codex rollout watcher that reads only newly appended log bytes and looks for
  quota/rate-limit failures (`rate limit exceeded`, `usage limit reached`, `HTTP 429 quota
  exhausted`, etc.).
- When detected, the app refreshes Codex quota and immediately runs the existing auto-switch runtime
  refresh path. This better matches the manual flow where switching right after a blocked session
  lets Codex compact/reconnect the existing task.
- Verification: `cargo test --lib` passed 75/75, `npm.cmd run build` passed, `npm.cmd run tauri
  build` passed, installed exe hash matched the release exe, and the descendant-process watcher
  still showed only `msedgewebview2.exe` children.

### 2026-07-11 - Compact token activity heatmap

- Changed Token activity from flexible columns that stretched across the card into fixed square
  cells: 10px daily/cumulative cells in seven rows and 14px weekly cells.
- Long activity ranges now scroll horizontally instead of stretching or compressing the heatmap.
- Rebuilt the Windows Tauri release and replaced the installed executable in
  `C:\Users\HP\AppData\Local\AI Account Switcher` so the compact heatmap is present in the app,
  not only in the source tree.
- Replaced the clipped per-cell pseudo-tooltip with a stable readout under the Token activity
  heading for both pointer hover and keyboard focus. Calendar cells now align to their weekday.
- Audited all activity modes: Daily keeps per-day values, Weekly aggregates token breakdown and
  cost by week, and Cumulative now accumulates token breakdown and cost together instead of showing
  cumulative total tokens beside single-day details. The cell container now exposes an accessible
  group of individually labelled buttons rather than hiding interactive cells inside an image role.
- Verification: `npm.cmd run build` passed, `cargo test --lib` passed 76/76,
  `npm.cmd run tauri build` passed, installed executable SHA-256 matched the release executable,
  and the live Tauri Usage tab verified the fixed-square calendar, focus readout, Weekly aggregation,
  and Cumulative totals (`940.46M` tokens / `$878.97`, matching the selected 30-day report).

### 2026-07-11 - Codex-style yearly activity and charcoal theme

- Rebuilt Token activity to match the Codex Profile layout: a full-width 53-week by 7-day calendar,
  month labels from August through July, compact Daily/Weekly controls, quantile-based intensity,
  and a clamped floating tooltip that remains visible at both calendar edges.
- Decoupled activity history from the 7d/30d/90d statistics request. The app now loads an all-time
  report for the yearly calendar, preventing older cells from being shown as zero when a shorter
  statistics range is selected. Future calendar days are visual placeholders, not interactive data.
- Removed Cumulative from the activity UI to match Codex, while retaining correctly aggregated
  Weekly token breakdown and cost with accessible labels for every interactive cell.
- Replaced the warm brown/orange theme with neutral charcoal surfaces, cool neutral typography,
  restrained periwinkle highlights, and a subtle grey selected-sidebar treatment. Semantic
  green/amber/red status colors remain unchanged.
- Verification: `npm.cmd run build` passed, `cargo test --lib` passed 76/76,
  `npm.cmd run tauri build` passed, the installed executable SHA-256 matched the release build, and
  the live Tauri app verified Daily, Weekly, the 7-day statistics filter with full-year activity,
  month labels, keyboard-focus tooltip (`81.32M tokens on Jul 11`), and edge clamping.

### 2026-07-11 - English-only Windows UI and runtime messages

- Converted the remaining Vietnamese Auto Session interface to English, including the sidebar
  subtitle, bulk schedule controls, account toggles, time inputs, result labels, log/statistics
  actions, table headings, extend prompt, and manual Prime now action.
- Converted user-visible backend validation errors, administrator-permission errors, Windows desktop
  notifications, manual-prime outcomes, and newly written Auto Session log lines to English.
- Kept the legacy `HOÃN` log marker as a read-only parser fallback so older log entries still count
  correctly in statistics; all new entries use `HOLD`.
- Verification: source audit found no remaining user-visible Vietnamese strings (excluding the proper
  name `Hoàng Phan`, internal comments, and the legacy parser marker), `npm.cmd run build` passed,
  `cargo test --lib` passed 76/76, `npm.cmd run tauri build` passed, installed/release executable
  SHA-256 matched, and the live Tauri Codex and Auto Session tabs displayed English-only UI.

### 2026-07-11 - Automatic fullscreen startup

- Configured the main Tauri window to enter native fullscreen automatically at startup. No custom
  fullscreen button or shortcut was added.
- Verification: `npm.cmd run build` passed, `cargo test --lib` passed 76/76,
  `npm.cmd run tauri build` passed, installed/release executable SHA-256 matched, and the live window
  capture covered the complete 1536x864 display from origin 0,0 with no title bar or taskbar.

### 2026-07-11 - F11 fullscreen and Codex-style dark title bar

- Replaced automatic startup fullscreen with a normal 1080x740 launch and an F11 fullscreen toggle.
  The custom title bar is hidden in fullscreen and restored when returning to windowed mode.
- Disabled the white native Windows frame and added a charcoal title bar with a draggable region,
  double-click maximize/restore, and Codex-style minimize, square maximize/restore, and close controls.
- Added the narrow Tauri window permissions required for close, minimize, maximize state/toggle, and
  fullscreen state/toggle.
- Verification: `npm.cmd run build` passed, `cargo test --lib` passed 76/76,
  `npm.cmd run tauri build` passed, installed/release executable SHA-256 matched, normal startup was
  captured at 1082x742 with the dark controls, and F11 entered native 1536x864 fullscreen with the
  custom title bar hidden. Final square icon polish was rebuilt and installed successfully.

### 2026-07-11 - Fullscreen viewport layout correction

- Added an explicit fullscreen shell layout instead of reusing windowed padding: the shell is fixed
  to the dynamic viewport, keeps a 14px top/side inset, removes bottom padding, and forces the
  workspace to consume the full remaining height.
- This prevents the Codex sidebar card from jumping against the top edge and removes the empty strip
  below the sidebar/content panel in fullscreen.
- Verification: `npm.cmd run build` and `npm.cmd run tauri build` passed, installed/release SHA-256
  matched, and the live 1536x864 fullscreen capture showed equal top alignment with both sidebar and
  content panel reaching the viewport bottom with no gap.

### 2026-07-11 - Luxury AS monogram and transparent app identity

- Replaced the previous orange app mark with an original brushed-platinum `AS` monogram across the
  custom title bar, sidebar footer, tray, taskbar executable, Windows tiles, and installer assets.
- Preserved the 1254px generated master in `brand/`, then isolated the monogram onto a true alpha
  background. Frontend uses an optimized 256px/38.9KB copy while Tauri icons are generated at each
  existing native size plus a multi-resolution transparent ICO.
- Verification: alpha extrema were 0..255, the 32px icon retained transparent pixels,
  `npm.cmd run build` passed with the optimized logo bundle, `cargo test --lib` passed 76/76,
  `npm.cmd run tauri build` passed, installed/release SHA-256 matched, and the live app showed the
  standalone silver monogram with the charcoal UI visible behind it in both title bar and footer.

### 2026-07-11 - Windows shell shortcut identity refresh

- Updated the user's OneDrive Desktop shortcut and per-user Start Menu shortcut to target the
  installed `ai-account-switcher.exe`, use its embedded transparent AS icon, and inherit the correct
  installation working directory.
- Requested a Windows shell icon refresh. The machine-wide ProgramData shortcut remains owned by
  the installer and could not be rewritten without administrator permission.
- Checked both Windows 11 context-menu layers for the installed executable. This Windows build does
  not expose `Pin to taskbar` for the file, so the protected Taskband registry data was deliberately
  left untouched; pinning can safely be completed from the running app's taskbar context menu.

### 2026-07-11 - Corrected stale Windows executable icon

- Root cause: the installed/release executable still dated from before the transparent AS ICO was
  generated, so the executable resource genuinely contained the previous app mark; this was not
  merely a Windows icon-cache issue.
- Forced a clean rebuild of the Tauri application package, replacing the installed executable with
  the new release binary after closing its stale running process. The installed and release hashes
  now match (`9890E9D59A5A6426655104AA3FB482C24FC9E5635F...`).
- Copied a dedicated `ai-account-switcher-as.ico` beside the installed executable and pointed the
  user Desktop and Start Menu shortcuts to it, then cleared Windows' icon cache, restarted Explorer,
  and reopened the application.
- Verification: `npm.cmd run tauri build` completed after the clean rebuild; the live application
  window title bar and sidebar displayed the silver transparent AS monogram from the newly installed
  executable.

### 2026-07-12 - Codex seamless quota handoff and reset-driven Auto Ping

- Replaced fixed-time Auto Session with a compact Codex-only Auto Ping view: global Auto Ping and Launch with Windows switches plus per-account Ping, status, Last, Next, and Ping now controls.
- Added a persisted reset-cycle scheduler with a 45-second startup settle, 3-minute grace, 10-second account spacing, single-flight execution, weekly pause, live reset confirmation, restart-safe dedupe, and bounded retry. Legacy `auto_prime` fields remain readable.
- Added official Tauri autostart using a current-user `--hidden` launch; normal launches still show the window and Close still hides to tray.
- Added persisted Codex checkpoint/switch/resume jobs. Checkpoints are bounded and redacted, and bind to the exact rollout path observed by the quota watcher. Candidate selection is deterministic, rejects API/login-broken/exhausted accounts, requires 5-hour usage below 95%, and verifies quota after switching. Resume runs exact `codex resume <SESSION_UUID> <PROMPT>` with selected `CODEX_HOME` and rollout cwd, never `--last`, and health-checks immediate process failure.
- Reviewer corrections applied: watcher-bound rollout selection, strict post-switch auth/quota validation, resume health-check, controlled retries, expanded secret redaction, fuller fallback checkpoint fields, and retryable no-candidate jobs.
- Verification: `cargo test --lib` passed 87/87, `npm.cmd run build` passed, and `npm.cmd run tauri build` produced MSI and NSIS bundles. Installed/release SHA-256 both equal `97C4A229FF8F6AE0838CAE85C635735DFFCE771835FF0CAE45FE8B25A3BD2D63`.
- Live verification: the installed Tauri Auto Ping tab showed both global switches and all required per-account controls in the charcoal/periwinkle theme. Normal launch exposed the window; controlled `--hidden` startup kept the process alive with no targetable window, then normal relaunch restored it. Live subscription ping/switch/resume was deliberately not triggered; those external flows were verified with pure decision/state tests and existing fake/local unit paths.

### 2026-07-13 - Windows Codex device-login code capture

- Fixed the hidden Windows `codex login --device-auth` output reader: it now reads output chunks instead of waiting for newline-delimited text, retains a bounded stream tail, and removes ANSI terminal control sequences before extracting the device URL and one-time code.
- This covers the Codex interactive terminal rendering that left the app on `Waiting for browser approval` without showing the nine-character code.
- Verification: added the styled/no-newline device-code parser test; `cargo test --lib` passed 88/88 and `npm.cmd run build` passed. `npm.cmd run tauri build` produced fresh MSI/NSIS bundles. Installed and release executable SHA-256 both equal `07A0156BF8DEF97EAC6687C0A1F7D53E7F12EE27CE35B8A134B318F4660F5472`; the installed app was restarted.
- Live limitation: the pending pre-fix login process was intentionally stopped to replace the executable, so a new sign-in must be started to observe a real one-time code. No credential or account profile was deleted.

### 2026-07-17 - Retired Auto Ping

- Removed the Auto Ping sidebar tab, global/per-account controls, Ping now/Prime now buttons, account-card prime status, frontend bridge calls, and the 45-second startup/60-second runtime scheduler.
- Removed the expiring-session prime reminder from the quota poller and unregistered the legacy Auto Session/prime/wake Tauri commands so the frontend cannot reconnect synthetic traffic accidentally.
- Preserved Launch with Windows as a separate Startup toggle in Settings because quota monitoring and seamless auto-switch still need the app process. Account profiles, credentials, active selection, quota data, and resume jobs were not modified.
- Added a startup retirement migration that forces `autoPingEnabled=false` and clears only the legacy `autoPing` behavior map. TDD evidence: the new retirement test first failed because the migration did not exist, then passed after implementation.
- Verification: `cargo test --lib` passed 89/89, `npm.cmd run build` passed, and `npm.cmd run tauri build` produced fresh MSI/NSIS bundles. Installed and release executable SHA-256 both equal `30ABB72C60D66563E2E05BDB0682475B87C199F8B597477324EB8BF4A350A28E`.
- Live verification: the installed app showed only Codex, Usage, and Settings in the sidebar; account cards had no Ping/Prime controls; Settings retained Startup; persisted state reported Auto Ping disabled with zero entries while all accounts remained present. No synthetic ping was executed during verification.

### 2026-07-18 - Onyx Luxury V2 sidebar and overlay scrollbar

- Redesigned only the left navigation as the user-approved Onyx Luxury V2: compact framed brand mark, stable Codex/Usage/Settings labels, consistent circular icon medallions, restrained lavender selection treatment, faint decorative monogram, and a one-line version/author footer. Removed the account-specific `Bare command = accN` copy from the sidebar while preserving all existing navigation handlers and the non-Windows API branch.
- Replaced the bright Windows scrollbar gutter on the main panel with a 7px translucent thumb, transparent track, and no arrow buttons. Independent review found the first rule was too broad; the final selectors are restricted to `.panel` and `.sidebarNav`, leaving modal, picker, and log scrollbars untouched.
- Verification gates: `npx.cmd tsc --noEmit` passed; `npm.cmd run build` passed (1,590 modules); `cargo test --lib` passed 89/89; `npm.cmd run tauri build` passed and produced both MSI and NSIS bundles. The repository has no lint script, so no lint gate was claimed. `git diff --check` passed. Rust emitted the existing retired Auto Ping dead-code warnings but no errors.
- Installation: the NSIS installer exited 0. Its extracted executable initially differed from the final post-bundle release hash despite the same length, so the stopped installed executable was synchronized from the final release artifact. Final release and installed SHA-256 both equal `747A46BB262C5040F5485F3C8304F1D4DAB6AC4226E3B65677E17CBF6842F246`.
- Live verification: the debug Tauri app was captured at the normal 1082x742 window and at an exact configured 820px content width (822px including the Windows border); the narrow view retained all sidebar labels/footer, moved account cards to one column, and had no horizontal overflow. The installed app was then launched and captured at 1082x742 with real account/quota state, the new sidebar, and the narrow overlay thumb visible at the right edge. No browser mock was used. A follow-up automated wheel/drag action was not claimed because Computer Use detected concurrent user input and stopped; account switching, quota refresh, Usage/Settings navigation, and the external author link were not exercised live because their logic was unchanged by this UI-only patch.

### 2026-07-19 - Codex end-to-end resume implementation
- Reworked the quota watcher to scan every rollout with newly appended data and bind the checkpoint to the exact rollout that emitted the quota error instead of assigning the newest shared-history rollout.
- Kept accounts with an unknown 5-hour window eligible when authentication is healthy and weekly quota remains, ranking them behind candidates with a known healthy 5-hour value. Post-switch verification applies the same rule; explicit quota errors can trigger handoff even while the 5-hour value is unavailable.
- Changed exact-session resume from a hidden child to a Windows console-backed `codex resume <SESSION_ID> <PROMPT>`. After the TUI survives its immediate health check, a delayed best-effort `/app` opens that same session in Codex App; the visible terminal remains the fallback.
- Verification: `cargo test --lib` passed 91/91, `npm.cmd run build` passed (1,590 modules), `npm.cmd run tauri build` produced MSI and NSIS bundles, and `git diff --check` passed after final formatting. Existing retired Auto Ping dead-code warnings remain unchanged.
- Installation: NSIS exited 0. The installed executable was synchronized to the final post-bundle release artifact; installed and release SHA-256 both equal `E6527DFAB5C2CC70BFCB13EECE8F92444F81BC3F4026884E760FA677F8AC7F46`.
- Live verification: the installed v0.5.5 window was launched and inspected with the real persisted account state. It showed acc5 active with weekly 71%, acc7 ready with weekly 0%, and the current promotional 5-hour values as unknown. No real quota exhaustion, account switch, resumed TUI, or `/app` handoff was triggered; those paths were verified by source-level state/selection tests and release execution only.

### 2026-07-21 - Isolated Codex login and automatic re-auth recovery
- Scope: keep Codex accounts independent from ChatGPT web logout by opening each Add/Re-login flow in an account-specific Chrome `--user-data-dir`; detect Codex 401s as expired login, expose Re-login, and automatically re-read quota after approval.
- Added a persisted per-profile `.reauth-pending` digest so an old revoked `auth.json` cannot satisfy the login watcher; after the file changes and quota succeeds, the marker is cleared and an active account's new credentials are synced to the default Codex/VS Code runtime.
- Verification: `cargo test --lib` passed 92/92, `npm.cmd run build` passed (1,590 modules), `npm.cmd run tauri build` produced MSI and NSIS bundles, and `git diff --check` passed. Existing retired Auto Ping dead-code warnings remain unchanged.
- Installation: NSIS exited 0. Installed and release SHA-256 both equal `A1BE0A0EC34A5E88E86F2C15D5E9B42ABD8AFA12000682D33AA85D37233836D1`.
- Live limitation: the installed app was launched successfully, but no real re-login was triggered; the current `acc8` HTTP 401 flow is covered by unit tests and the new UI path, while the user must click Re-login and approve the device code once to exercise the external OAuth step.

### 2026-07-21 - Always show the Codex login email on account cards
- Kept the existing card email display and hardened backend identity extraction: read the login email from the Codex `id_token`, with an `access_token` claim fallback when the provider omits the id token email claim. No token contents are sent to the UI.
- Verification: `cargo test --lib` passed 93/93, `npm.cmd run build` passed (1,590 modules), `npm.cmd run tauri build` produced MSI and NSIS bundles, and `git diff --check` passed.
- Installation: NSIS exited 0. Installed and release SHA-256 both equal `52B24FED41774FDFA20196B4BFFFFFB13F2DC18CE59EB7F39CDEA92C1B4CF371`.
- Live limitation: the installed app was restarted with real account state; email extraction was not forced through a new login because that would require changing the user's OAuth session. Existing cards with valid tokens display the login email, and missing-id-token coverage is unit-tested.

### 2026-07-21 - Heal persisted Codex 401 state on startup
- Added a targeted startup migration for non-default, non-API Codex profiles whose persisted quota error requires re-authentication. These accounts now enter `NeedsLogin` on restart instead of incorrectly retaining `Idle`/`Active`; machine-default authentication remains outside this recovery path.
- Verification: the two focused migration tests passed, scoped `rustfmt --check` passed for `app_state.rs`, the full Rust suite passed 95/95, `npm.cmd run build` passed (1,590 modules), and `npm.cmd run tauri build` produced fresh MSI and NSIS bundles. Existing retired-feature dead-code warnings remain unchanged.
- Installation: same-version MSI replacement was blocked (`1603`, then policy/elevation code `1625`); per-user NSIS installation exited 0. The stopped installed executable was then synchronized from the final post-bundle release artifact. Installed and release SHA-256 both equal `E291D9F7B459101342E5C4D711E4B98D8A3FDE0BAA7FED4E8D5F7F9D6E9FFBC3`.
- Live verification before the migration showed all five real cards and their login emails, including `acc8` with `HTTP 401` but an incorrect `Ready` badge. After the release restart, `acc1`, `acc4`, `acc9`, and `acc91` still showed their emails, but `acc8` was absent from both the persisted account list and its profile directory. No Re-login, Delete, account switch, or OAuth approval was clicked during verification. The external re-login flow therefore remains unverified live, and `acc8` must not be reconstructed automatically without user approval because its original profile/credential data is no longer present.

### 2026-07-21 - Resilient Codex quota refresh fanout
- Fixed the visible multi-account quota timeout failure by bounding Codex quota fanout to three profiles at a time and retrying only transient transport/timeout/server failures once after a short delay. HTTP 401/auth failures still do not retry as network noise.
- Hardened Codex OAuth token refresh so the refresh request has a 15-second timeout and preserves failure class: transient network/server refresh errors remain retryable and do not force `NeedsLogin`, while rejected 400/401 refresh tokens still require re-authentication. Token/body contents are not surfaced to the UI.
- Added a stale-quota fallback for transient refresh failures: if an account already has known quota values, the app keeps those values and shows a small warning instead of replacing the whole card with a timeout error. Auth failures still replace the previous quota so revoked accounts are not hidden.
- Verification: targeted RED/GREEN tests covered transient quota retry, no retry on 401, batched seven-account refresh, transient token-refresh timeout, rejected refresh-token reauth, and stale-quota fallback. `cargo test --lib --quiet` passed 102/102, `npm.cmd run build` passed (1,590 modules), `rustfmt --edition 2021 --check src-tauri/src/app_state.rs src-tauri/src/quota.rs` passed, `git diff --check` passed, and `npm.cmd run tauri build` produced fresh MSI and NSIS bundles. Existing retired-feature dead-code warnings remain unchanged.
- Installation: per-user NSIS installation exited 0. The stopped installed executable was synchronized from the final post-bundle release artifact. Installed and release SHA-256 both equal `002D51C3CE8A551295BF00AEDBAF924ABEBA6B97204CCBE8F1A4B0C1AF662CB3`.
- Live verification: the installed v0.5.5 app was launched and inspected via the real Windows UI. Before refresh, `acc2` showed the previous network timeout. After clicking the real header `Refresh quota` button, the button disabled during refresh, the run completed, and all seven visible accounts showed quota/status with no visible `Couldn't read quota` or `Network issue`: `acc1` 100%, `acc2` 100%, `acc3` 10%, `acc4` 100%, `acc7` 98%, active `acc8` 5%, and `acc9` 99%. No account switch, delete, login approval, or Codex quota exhaustion was triggered.

### 2026-07-21 - Final installed verification for resilient quota cards
- Rebuilt after adding the stale-quota presentation, then installed the final NSIS release. Installer exit code was 0; installed and post-bundle release executable SHA-256 both equal `28AC79245AD1966040FAC938097699E4CB1E153901B72E752186F69078DD3FE2`. The NSIS installer SHA-256 is `B0EED79D30FBB5C8FE07F29FEC6EF1E089107B1F3E628EB86E125D958A21BAC7`.
- Live verification: startup refresh completed with quota visible for all seven real accounts. A second real header `Refresh quota` run also completed with all seven cards populated: `acc1` 100%, `acc2` 100%, `acc3` 10%, `acc4` 100%, `acc7` 98%, active `acc8` 6%, and `acc9` 99%. No card showed a network/auth error after either completed run.
- Simulated/test-only verification: forced transient timeouts preserve the prior values and show `Network issue; showing last known quota`; forced HTTP 401 replaces prior values and remains a re-authentication failure. The final live runs did not happen to produce a timeout, so the yellow stale-data warning itself was not observed live.

### 2026-07-21 - Wabi Quiet Luxury sidebar navigation
- Replaced the rounded-card sidebar navigation with the approved Concept C treatment: a centered unboxed brand header, warm serif labels, compact `01`/`02`/`03` indices, a lavender dot and short underline for the selected destination, and no selected background or left rail. Preserved every existing route and the conditional non-Windows API entry.
- Reworked the lower sidebar into a quiet editorial footer with a large faint `AS` monogram and centered `v0.5.5 · Hoàng Phan` signature. Added `aria-current="page"` to the active navigation item after independent review identified the missing programmatic selected state.
- Verification: `npx.cmd tsc --noEmit` passed; `npm.cmd run build` passed (1,590 modules); `cargo test --lib --quiet` passed 102/102; `git diff --check` passed; and `npm.cmd run tauri build` produced fresh MSI and NSIS bundles. The existing retired Auto Ping dead-code warnings remain unchanged.
- Installation: the NSIS installer exited 0. Final release and installed executable SHA-256 both equal `0FB5C55A5DBEBF4D020F978061EEEFA284A39568047179CB44DAEB6FEE78B719`; the NSIS installer SHA-256 is `42DD99F62BFC7D3EF9825191DE8E86156BE14CBBB5ECD6785DA5F9740DDB9D5F`.
- Live verification: the installed Windows app was opened with real persisted data and navigated through Codex, Usage, and Settings. Header alignment, numbering, lower monogram/footer, and all three selected states were visually checked at the real 224px sidebar width. The first live pass exposed a global hover rule that restored a rectangular selected background; the selector was tightened, rebuilt, reinstalled, and each destination was checked again with only the approved underline/dot treatment. No browser mock or simulated UI was used for this final visual verification; account actions and quota behavior were outside this UI-only change.

### 2026-07-21 - Sidebar typography and bounded Token activity width
- Replaced Georgia only within the three sidebar navigation rows with the installed Windows `Segoe UI Variable` family. Navigation labels now use a restrained 15px semibold treatment and subtitles use 11px regular; the approved brand and footer typography were intentionally left unchanged.
- Prevented the 53-column Token activity heatmap from stretching across a fullscreen panel by keeping its responsive `100%` behavior below the design width, capping the calendar and matching header at 960px, and centering the bounded block when more space is available.
- Verification: `npx.cmd tsc --noEmit` passed; `npm.cmd run build` and the final `npm.cmd run tauri build` passed with 1,590 modules and produced fresh MSI/NSIS bundles; `git diff --check` passed. The existing 83 retired-feature dead-code warnings remain unchanged.
- Installation: the final NSIS installer exited 0. Because the same-version installer retained the prior executable, the stopped installed binary was synchronized from the final post-bundle release. Final installed and release SHA-256 both equal `97F539DBEE68E4D07766792CF06E9A3AB5F92FCB904C8F6517C3CDF4C3B69304`; the NSIS installer SHA-256 is `F261562A1E5B7E2E2797D74BBE1528525D08CC14934AAE5E2BF294DB82812808`.
- Live versus bounded verification: the final installed binary's new sidebar typography was inspected with real account data. An intermediate 1040px heatmap cap was inspected live at both the normal 1082x742 window and maximized size and was confirmed to stop viewport-wide stretching; that review motivated the tighter final 960px cap. The final 960px release was built and installed, but its Usage view was not re-captured because Computer Use detected concurrent user input and stopped. The final cap is therefore source/build-verified, not claimed as live-observed.

### 2026-07-24 - Codex device-code and EDU browser-profile sign-in
- Added a Codex-only sign-in choice in Add account: `Device code` preserves the existing `codex login --device-auth` flow, while `Chrome profile (EDU)` opens the account's persistent isolated Chrome data directory and starts standard `codex login` without `--device-auth`. The selected method is persisted on the account and reused by Re-login.
- Security boundary: the browser-profile route keeps OAuth credentials in the existing per-account `CODEX_HOME` directory, does not change the system default browser association, does not intercept OAuth callbacks, and does not write browser/session tokens into app UI or logs. The Codex CLI remains responsible for its browser callback.
- Verification: `rustfmt --edition 2021 --check` passed for changed Rust files; `cargo test --lib --quiet` passed 103/103, including a new assertion that browser-profile login is exactly `codex login` and device-code remains `codex login --device-auth`; `npx.cmd tsc --noEmit`, `npm.cmd run build`, `git diff --check`, and `npm.cmd run tauri build` all passed. The Tauri build emitted the repository's existing retired-feature dead-code warnings.
- Artifacts: fresh release executable SHA-256 `469A1753C6E9BABBDB805741F1DECE129E280620014809CFEFA8B3EC4A833BBA`; NSIS setup SHA-256 `624F35409141137B8DD16C3F78D490266C99C0359D9766C530DF1D4488B0CD74`. The installed app was still running, so it was deliberately not stopped or overwritten; its prior SHA-256 remained `97F539DBEE68E4D07766792CF06E9A3AB5F92FCB904C8F6517C3CDF4C3B69304`. No live browser OAuth, EDU workspace approval, or installed UI click was performed in this pass.

### 2026-07-24 - Installed release check for dual Codex sign-in
- The running installed v0.5.5 process was intentionally stopped after user approval, the NSIS setup completed with exit code 0, and the post-bundle release executable was copied into the installed location to avoid the same-version installer retaining an older executable.
- Installed and release SHA-256 both equal `469A1753C6E9BABBDB805741F1DECE129E280620014809CFEFA8B3EC4A833BBA`; the relaunched process (PID 15468) remained alive and responding after five seconds. A direct window capture showed the real Codex account view and its `Add account` control.
- Live boundary: Windows foreground focus was repeatedly reclaimed by the user's active Chrome/Gmail window. The automated modal click was therefore stopped before any account creation, browser OAuth, or EDU workspace action. The new Add-dialog selector and an actual EDU sign-in remain source/build/unit-verified, not claimed as live-click-verified.

### 2026-07-24 - Tokyo luxury board sidebar theme
- Reworked the Windows Codex-only shell toward the user-approved board: the left rail is now a single integrated sidebar surface with a Tokyo night illustration, warm gold/red materials, serif navigation labels, circular numeric indices, a vertical kanji accent, and a minimal note-style footer instead of the prior personal credit link.
- Kept the existing account/quota logic unchanged. The only source changes were `src/App.tsx`, `src/styles.css`, and the new workspace asset `src/assets/sidebar-yoru-lane.png`. Added explicit header button treatments so `Add account` and `Refresh quota` visually match the approved board more closely.
- Verification: `npx.cmd tsc --noEmit` passed and `npm.cmd run build` passed (Vite production bundle, 1,590 modules). The generated sidebar artwork was included in the bundle as `dist/assets/sidebar-yoru-lane-*.png`.
- Verification boundary: this pass is source/build-verified only. No fresh Tauri package, installer sync, or live installed-window inspection was performed in this pass.

### 2026-07-24 - User-requested rollback of Tokyo shell iteration
- Rolled the working tree back to the saved pre-integrated-shell snapshot `861e05a` (`wip: save current tokyo sidebar draft`) at the user's request. Removed only the new untracked trial background asset created during the rejected iteration; existing untracked `.playwright-cli/`, `output/`, and `tmp/` directories were preserved.
- Verification: `npx.cmd tsc --noEmit` passed. The Tauri bundle command stalled without further output after the release executable had been rebuilt, so no MSI/NSIS bundle success is claimed for this rollback pass. The freshly produced release executable was copied to the installed location; release and installed SHA-256 match, and the restarted app process was alive and `Responding = True` with window title `AI Account Switcher`.
### 2026-07-25 - Corrected rollback: remove Tokyo background only
- The prior rollback target still contained the Tokyo sidebar theme. Restored only `src/App.tsx` and `src/styles.css` from the parent of the Tokyo UI commit (`861e05a~1`) and removed `src/assets/sidebar-yoru-lane.png`. This preserves the intervening Codex browser-profile login and quota-related implementation.
- Verification: `npm.cmd run build` passed and no Tokyo sidebar asset was emitted into `dist/assets`; `cargo build --release` passed with the repository's existing 83 retired-feature dead-code warnings. The refreshed release executable was copied to the installed location; release and installed SHA-256 match, and the restarted `AI Account Switcher` process was alive and `Responding = True`.
### 2026-07-25 - Corrected rollback application packaging
- Root cause of the failed launch: the rollback pass copied an executable produced by direct `cargo build --release`, which behaved as a development build and attempted the configured `http://127.0.0.1:5173` frontend. It must not be used as the installed Tauri executable.
- Rebuilt through `npm.cmd run tauri build`; production Vite build plus MSI and NSIS bundles completed successfully. The post-bundle release executable was copied to the installed location. Release and installed SHA-256 match; the relaunched app was alive and `Responding = True` with a nonzero main-window handle. No newly launched Chrome process targeted the dev URL.
### 2026-07-25 - Japanese noir unified shell implementation
- Added the approved unified-shell UI treatment: sidebar and content now share one bounded surface, with the Tokyo rainy-lane artwork confined to the sidebar and dark gradient overlays preserving quota readability.
- Removed decorative Japanese vertical text and circular/filled sidebar index treatment; `01`, `02`, `03` remain plain unboxed indices. The existing `LogIn` icon for `Add account` was preserved unchanged.
- Added restrained warm-ivory hairlines, vermilion active marker, serif navigation titles, subtle paper/grain texture, and low-contrast logo etching. Account/quota behavior was not changed.
- Verification: `npm.cmd run build` passed (`tsc` plus Vite production build, 1,590 modules). The production bundle includes `sidebar-yoru-lane-B0PNKoib.png` at 2,067.14 kB. No live installed Tauri window inspection was performed in this UI-only pass.

### 2026-07-25 - Applied Japanese noir UI to installed Windows app
- Ran `npm.cmd run tauri build`; production Vite build passed, Rust release build passed with the repository's existing 83 dead-code warnings, and MSI/NSIS bundles were generated.
- Replaced the installed executable at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe` with the release executable from `src-tauri\target\release\ai-account-switcher.exe`.
- Live verification: release and installed SHA-256 both equal `4F5B0713861833429975CC586353E3F7033A2E700C247619BB6C5933B59BB3D5`; process PID `13612` remained alive, `Responding = True`, title `AI Account Switcher`, nonzero main-window handle `133086`.
- Live visual inspection: unified shell, Tokyo sidebar background, plain sidebar indices, red active marker, quota cards, and preserved Add account control are visible in the installed app. No account/login action was clicked.

### 2026-07-26 - Japanese noir full-window canvas and integrated transparent sidebar
- Added the user-supplied 1672x941 global canvas as `src/assets/japanese-noir-canvas.png` and the supplied 718x2188 narrow atmosphere as `src/assets/sidebar-noir-atmosphere.png`; the prior asset was preserved.
- Scoped the visual change to `src/styles.css`: the canvas now runs behind the titlebar, sidebar, and content; the narrow atmosphere is dimmed and masked into the far-left edge; workspace/sidebar card borders and rounding were removed; the active navigation state is reduced to a 2px lacquer-red marker without fill or glow. React structure, account logic, buttons, alerts, cards, and Auto Ping behavior were not changed.
- Verification: fresh `npm.cmd run build` passed (`tsc` plus Vite production build, 1,590 modules). Vite emitted both supplied assets in the production bundle. A browser-mode visual render was attempted but the existing direct-browser Tauri `getCurrentWindow()` metadata dependency stopped React before paint; no source workaround was added because that would expand this UI-only task. No installed Tauri binary was built, replaced, or live-tested.

### 2026-07-26 - Installed unified background and transparent-sidebar production build
- Confirmed rollback checkpoint `dff12d60c5f24d7e3b05838fb60b34f30885c28d` before installation; its tree matches the pre-implementation parent and the implementation remains unstaged.
- Fresh `npm.cmd run build` passed (`tsc` plus Vite, 1,590 modules). Fresh `npm.cmd run tauri build` passed, including the production frontend, optimized Rust executable, MSI, and NSIS; the existing 83 retired-feature dead-code warnings remain.
- Replaced only `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe` with the post-Tauri-build release. Built and installed SHA-256 both equal `A59E3F0DD1BD0F2C6AF3E7A1B83DBD013C1F5D58952A538309B08C1634AA1336`.
- Live verification: PID 24520 remained alive and responding with title `AI Account Switcher` and nonzero window handle 198634. Visual inspection confirmed the full-window dim Japanese-noir canvas, transparent integrated sidebar with faded left atmosphere, no rounded sidebar/workspace card, thin lacquer-red active marker, and readable unchanged dashboard header/cards/buttons/account controls. No account or settings action was clicked.

### 2026-07-26 - Corrected global canvas visibility and footer credit
- Removed only the sidebar footer separator and `Hoàng Phan` text from `src/App.tsx`; the `v0.5.5` label, footer wrapper/control, red dot, divider, alignment, and click behavior remain.
- Corrected the global-canvas layering in `src/styles.css`: reduced both `.shell` darkening gradients, lowered `.topbar` opacity, replaced the `.workspace` fill with a very-low-opacity gradient, and replaced the opaque `.panel` fill with a directional translucent content veil. Card backgrounds/layout/content were not changed. The narrow atmosphere remains isolated to `.sidebar::before` with its left-to-right mask.
- Verification: fresh `npm.cmd run build` passed and fresh `npm.cmd run tauri build` produced the optimized executable plus MSI/NSIS with the existing 83 dead-code warnings. Built and installed SHA-256 both equal `576058A148261476EBA64A3EAD49142F05C3A998FEC1317096E9C4B341D7CE7C`.
- Live visual verification: the installed app was relaunched and inspected. The wide canvas is visibly continuous through the titlebar and main header/negative space; the sidebar atmosphere fades into it; cards remain dark/readable; the footer displays only `v0.5.5`. No account/settings action was clicked.

### 2026-07-26 - Prevented false account switches when creating a Codex chat
- Root cause: the 10-second rollout watcher treated any newly appended rollout line containing quota/rate-limit wording as a real exhaustion signal. Normal user messages, assistant text, and tool outputs in a newly created chat could therefore invoke auto-switch.
- Hardened the detector to ignore JSON rollout content unless it is an `event_msg` carrying an explicit error signal (`error`, `last_error`, `type=error`, or `type=turn_aborted`). Plain non-JSON quota errors remain supported.
- TDD evidence: the new-chat regression test failed against the old predicate, then passed after the fix. Full Rust library suite passed (`104 passed, 0 failed`); `npx.cmd tsc --noEmit`, `npm.cmd run build`, and `git diff --check` passed.
- Packaging/live process evidence: `npm.cmd run tauri build` produced the optimized executable plus MSI/NSIS. The release executable was copied into the installed location; release and installed SHA-256 both equal `21C08C4F568FD11A4A4CE44ECA1CD0899770DC50E7AAFE104464C23B6801BA53`. The relaunched installed process (PID 16648) was responding with a nonzero main-window handle.
- Verification boundary: false-positive and genuine-error detection are unit-tested/simulated. The installed executable and process health are live-verified, but no real quota exhaustion was induced and no live VS Code new-chat reproduction was performed in this pass.

### 2026-07-26 - Full bug sweep: ten defects fixed across backend, gateway, and UI
- Data safety: Windows `create_symlink` was a silent no-op while `link_shared_entry` had already deleted the profile's real config entry, so every app start destroyed profile `config.toml`/`skills` with nothing put back. Implemented real Windows file/dir symlinks, probed once per run, and made the destructive replace conditional on symlink support. `state.json` writes are now serialized behind a process-wide lock with a per-process staging name and a `.bak` of the last good state that `load()` falls back to, so concurrent gateway writers can no longer splice the file and a corrupt state no longer panics startup. `api_usage.json` gained the same stage-plus-rename treatment plus a write lock; Codex `auth.json` token rotation now retries a locked rename and falls back to an in-place write so a transient file lock can no longer discard a rotated refresh token and force re-login.
- Correctness: `delete_account` now derives "was active" from `active/<tool>.profile` (via `active_account_id_for`) instead of `state == Active`, so deleting an exhausted-but-active account clears the pointer instead of leaving bare `codex` in a deleted directory. The gateway retry set is keyed by `(account, model)` so a combo can fall back to the same account's other model. The rollout watcher commits its cursor only over bytes actually consumed, cuts reads at line boundaries (lossy UTF-8, 4 MB cap) so a mid-character tail can no longer abort the pass and permanently skip a real quota error; handoff/quota rollout readers now scan bounded head+tail windows instead of loading multi-hundred-MB files whole. Auto-switch gained a reentrancy flag and a "must improve on current usage" candidate rule, ending one-event cascades through every account in the 92–95% band. A shrunken usage JSONL now rebuilds the whole cache instead of double-counting.
- Gateway/UI: `stream: true` responses are now forwarded incrementally via `Body::from_stream` with a final-line flush (previously the entire upstream body was buffered, then dumped, losing any unterminated final event). Cooldowns are rehydrated from persisted timestamps on gateway start; API key comparison is constant-time. Frontend: `getCurrentWindow()` is guarded so plain-browser dev renders again; the quota poller keys on tool ids instead of the snapshot array it produces (previously a tight refresh loop while focused); modal submit buttons are single-flight; re-fetching gateway models re-anchors the selected model; server settings (port/cutoff/rotation) are locked while the gateway runs instead of silently not applying; the port is validated; the login-code toast moved out of the state updater (StrictMode duplicate).
- Verification: `cargo fmt` on changed files; full Rust suite passed 118/118 (104 existing + 14 new regression tests covering symlink data-loss, store corruption/concurrency, active-file detection, combo fallback, tail reads, candidate headroom, cache invalidation, auth persistence, stream flush, key comparison); `npx.cmd tsc --noEmit` and `npm.cmd run build` passed; browser-mode dev server rendered the full mock UI with zero console errors (previously white screen). No installed Tauri binary was replaced in this pass.

### 2026-07-26 - Installed the ten-defect bug-sweep build
- Committed the pending work as `a2e9d1b` (japanese noir canvas/sidebar UI) and `91491cb` (ten-defect bug sweep) on branch `fix/bug-sweep-2026-07-26`, then ran a fresh `npm.cmd run tauri build`: production Vite build, optimized Rust executable, MSI, and NSIS all completed with the repository's existing 83 retired-feature dead-code warnings.
- Stopped the running installed process (PID 16648) and replaced `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe` with the post-bundle release executable. Built and installed SHA-256 both equal `AAC8B04827E65199804F5EA7C0CCAB78CEED040E4C7B5F79393248143AA8D760`.
- Live verification: relaunched process PID 22820 is alive, `Responding = True`, title `AI Account Switcher`, nonzero main-window handle 263374. No account or settings action was clicked in this pass.

### 2026-07-26 - Second bug-sweep build installed (round 2 fixes)
- Committed `a5fff6c` (second bug-sweep) on branch `fix/bug-sweep-2026-07-26`. Seven further defects from a second-pass adversarial audit, all backend (`api_gateway.rs`, `tools.rs`), each with a regression test: UTF-16 PowerShell profile no longer wiped; startup heal no longer restores a stale Codex `auth.json` over rotated tokens; PowerShell hook resolves the OneDrive-redirected Documents root; refresh/resume SendKeys gated on the target window actually being foreground; Responses->client stream no longer duplicates the answer or leaks reasoning; chat tool messages translated to Responses `function_call`/`function_call_output`/`input_text` items; Anthropic->Responses now finalizes tool calls and carries usage, and reasoning items no longer become empty Anthropic messages; graceful-shutdown drain bounded (3s) and upstream client given connect(15s)/read(300s) timeouts.
- Verification before build: `cargo test` = 126 passed / 0 failed (8 new tests over the previous 118); `npx tsc --noEmit` clean; `npm run build` clean. Retired-feature dead-code warnings unchanged in family (83-84).
- Ran `npm.cmd run tauri build`: production Vite build + optimized exe + MSI + NSIS all succeeded. Stopped the running process (PID 22820), replaced `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe`. Built and installed SHA-256 both equal `206F7286B1349A7351CC39E24E2095212D0F502558F59B9F7AFF7ECD55B84CA5`.
- Live verification: relaunched process PID 22436 is alive, `Responding = True`, title `AI Account Switcher`, main-window handle 526256. No account/settings action was clicked in this pass.

### 2026-07-27 - Claude-on-Windows feature + round-2 finish + dead-code removal
- Committed `159894d` on branch `fix/bug-sweep-2026-07-26`. Three parts, per the approved plan:
  - **Part A — Claude Code accounts on Windows.** `visible_tools()` now returns `[Claude, Codex]` on Windows. New `quota::claude_has_credentials` (macOS Keychain OR `.credentials.json`) is used by `tools::profile_has_credentials` (was Keychain-only, so a finished Claude login was never confirmed on Windows and the account stayed `NeedsLogin`) and by `detection.rs`. The macOS `security` delete in `delete_account_files` is now `#[cfg(target_os = "macos")]`. Frontend: removed the dead `isWindowsCodexOnly` gate so Claude + the API tab appear on Windows. The rest of the account machinery (launchers, PowerShell hook, tray, detection, login-terminal) was already tool-agnostic.
  - **Part B — five defects from the finished round-2 sweep** (the earlier run was cut short by a session limit; re-run finders + adversarial verify). Fixes, each with a test where unit-testable: streaming passthrough no longer corrupts multi-byte UTF-8 split across network chunks (forward raw bytes); a transient quota-read failure no longer flips an Exhausted account to Idle (extracted `state_after_refresh`, keep prior state when the cycle is unread); editing the gateway URL/key clears fetched models; Usage streaks compute over full history not the range window; `read_file_tail_from` no longer stalls on a >4MB partial first line.
  - **Part C — removed the retired subsystems** (auto session priming, pmset wake helper, auto-ping): deleted `wake.rs`, `prime.rs`, `auto_ping.rs`, their unregistered Tauri commands, DTOs, orphaned helpers and dead tests. Compiler warnings **83 → 0**, no runtime behavior change.
- Verification before build: `cargo test` = **111 passed / 0 failed**; `cargo build --lib` warnings **0**; `npx tsc --noEmit` clean; `npm run build` clean.
- `npm.cmd run tauri build`: production Vite build + optimized exe + MSI + NSIS all succeeded. Stopped the running process (PID 22436), replaced `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe`. Built and installed SHA-256 both equal `E70678013A81DEB0A5E03E62C2D97E6A47237E4AE1CFD449B294785114518F36`.
- Live verification: relaunched process PID 6944 alive, `Responding = True`, title `AI Account Switcher`, nonzero main-window handle. Not automated: clicking through the native webview to visually confirm the Claude tab, and a real Claude OAuth login (needs the user's browser sign-in). The Claude-visibility logic is covered by the compiled `visible_tools()` Windows path and the `claude_has_credentials` unit test.

### 2026-07-27 - Accounts Black Chrome / Platinum Silver workbench themes
- Redesigned only the Accounts screen around a VS Code/JetBrains-style workbench hierarchy: separate title bar, sidebar, workspace, panel/card, and focus layers. Black Chrome is the default; a compact, accessible Sun/Moon segmented control switches the Accounts screen to Platinum Silver and persists the choice in `localStorage`. Account, quota, API gateway, and Tauri backend workflows were not changed.
- The theme CSS is scoped by `accountsTheme` on the Accounts view. It replaces the noisy noir canvas there with neutral black/silver surfaces, a thin selected-nav rail, neutral plan badges, semantic quota colors, focused active-card borders, and IDE-like primary/secondary action buttons. Usage, API, and Settings retain their prior visual treatment.
- Verification: `npx.cmd tsc --noEmit`, `npm.cmd run build`, and `git diff --check` passed. `npm.cmd run tauri build` produced a fresh optimized executable plus MSI/NSIS bundles. The release and installed executable SHA-256 both equal `5D2915E747C23A1E2A753E9124972B48932F22F72370E3FBF72D4168B0571344`; the replaced installed process relaunched as PID 11456, `Responding = True`, title `AI Account Switcher`, with a nonzero window handle. The browser-only Vite fallback could not populate account cards because existing unguarded Tauri event listeners throw outside the Tauri shell; that pre-existing limitation was not changed. Native visual clicking of the new toggle was not automated in this pass.

### 2026-07-27 - Accounts/browser and auto-switch safety bug fix
- Fixed the browser/dev fallback: all frontend native-event subscriptions now pass through a Tauri-aware helper, so mocked browser mode never calls the unavailable event IPC bridge. The Codex login dialog and footer link also fall back to `window.open` when not inside Tauri. Browser verification rendered the full mock Accounts view; both Black and Platinum Silver controls were clicked and their accessible pressed state updated. The only browser-console error was the non-functional missing `/favicon.ico` request.
- Fixed Codex quota rotation: an ordinary quota refresh with no exact active rollout no longer returns early from the seamless-resume path. It falls through to the normal account switch, while an active handoff remains protected from re-entrant switching. Added the regression test covering the no-rollout fallback.
- Hardened API gateway selection against account/model mismatches. A registry is now treated as an account-scoped allowlist once the provider has a registry; the heuristic is used only before the first registry refresh. Direct model resolution likewise requires a live account that advertised that model. Added a regression test where `a2` must not receive `claude-1` traffic advertised by `a1` only.
- Verification: `npx.cmd tsc --noEmit`, `npm.cmd run build`, `cargo check --lib`, `cargo test --lib --no-run`, and `git diff --check` passed. A targeted Rust test binary was compiled but Windows failed to start it with `0xc0000139 (STATUS_ENTRYPOINT_NOT_FOUND)` before assertions could run; no OS/DLL modification was attempted. `npm.cmd run tauri build` refreshed the release executable, MSI, and NSIS setup. Release and installed SHA-256 both equal `9D3094AA465F2AD8085E65FEE32ED671DDEAE8BC30D99F7BFE0FE4BC376821FF`; the installed app was replaced and relaunched as PID 15928, `Responding = True`, title `AI Account Switcher`, window handle 329730. No account, login, or gateway request was executed.

### 2026-07-28 - Unified Black Chrome / Platinum Silver theme across all tabs
- Fixed the reported tab regression in `src/App.tsx`: the workbench theme class is now attached to the whole shell instead of only the Accounts view. API, Usage, and Settings therefore keep the selected Black Chrome or Platinum Silver surfaces when navigating from the sidebar. The appearance toggle moved into the title bar so it remains available on every tab.
- Runtime RED: before the fix, clicking API produced `main.className === "shell"`, which activated the old Japanese-noir presentation. Runtime GREEN: Playwright verified API, Usage, and Settings each retain `shell accountsTheme accountsTheme--silver`; the silver toggle stayed pressed across navigation. Visual checks captured the updated Usage and API views.
- Verification: `npx.cmd tsc --noEmit`, `npm.cmd run build`, and `git diff --check` passed. `npm.cmd run tauri build` produced the updated release executable and MSI; `npx.cmd tauri bundle --bundles nsis` refreshed the NSIS installer. Release and installed SHA-256 both equal `44EA94F1082EE1ABC47C476586484EC2680AD0E57369E0F97DF089C0B3D00336`; the installed app was replaced and relaunched as PID 11868, `Responding = True`, title `AI Account Switcher`, window handle 133274. No account, login, API server, or gateway request was executed.

### 2026-07-28 - Audit hardening sweep: responsiveness, security, Usage, dependencies, and installed verification
- Tauri responsiveness: moved the confirmed heavy UI-triggered commands (`fetch_gateway_models`, `get_usage`, `start_api_gateway`, `stop_api_gateway`, `refresh_api_gateway_models`, plus quota refreshes) to async command handlers backed by `tauri::async_runtime::spawn_blocking`, so slow HTTP/log-scan/gateway shutdown work no longer runs on the webview command path.
- Dependency/security hardening: updated the npm lockfile to audited `vite 6.4.3` and `postcss 8.5.23` (`npm.cmd audit --audit-level=moderate` reports 0 vulnerabilities), removed the unused Tauri dialog plugin/capability and clipboard read permission, and configured explicit production/dev CSP in `tauri.conf.json`.
- Secret/metadata hardening: `Store::write_state` now applies owner-only permissions to staged state, backup, and final `state.json` on Unix while preserving the existing Windows ACL behavior; API gateway `/health` now returns readiness only (`{"ok": true}`) without unauthenticated combo/key counts.
- Usage/Favicon: the Usage tab now makes one `get_usage` call that returns selected-range totals plus the all-time activity source from one backend cache refresh; the activity calendar is labeled `Last 12 months`; `index.html` now declares the app favicon.
- Windows test-loader fix: Rust tests now run normally again (`cargo test` = 114 passed plus 0 doctests). The build uses an explicit Tauri feature set and disables bin tests for the Tauri launcher entrypoint, avoiding the previous `0xc0000139 (STATUS_ENTRYPOINT_NOT_FOUND)` test-start failure.
- Verification: `npm.cmd run test:audit` passed 6/6; `cargo test --lib` passed 114/114; `cargo check --all-targets` passed; `npm.cmd run build` passed; `npm.cmd audit --audit-level=moderate` reported 0 vulnerabilities; `cargo test` passed 114/114 plus doctests; `git diff --check` passed; `npm.cmd run tauri build` produced the release executable plus MSI/NSIS bundles.
- Installed app: backed up the previous executable to `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260728-audit-fix`, replaced only the installed executable, and relaunched the app. Final built and installed SHA-256 both equal `A48EA8E6E56545205F9051C43BA8C08497A71374104A3499D51209679627A2EE`; live process PID 2432 is `Responding = True`, title `AI Account Switcher`, window handle 329904. No account/profile directory, login flow, or gateway request was modified or exercised.
- Live installed smoke test: Accounts, API, Usage, and Settings retained the redesigned UI in both Black Chrome and Platinum Silver; Usage loaded successfully and displayed `Token activity · Last 12 months`. The appearance was restored to Silver after testing. At the real app-data path, `state.json` and its immediate backup both contained the same 8 account identities, 6 profile directories remained present, and the Windows ACL remained limited to the current user, SYSTEM, and Administrators. No account action, login, gateway start, or gateway traffic was triggered.

### 2026-07-28 - Settings switch and Usage contrast polish
- Fixed the reported UI defects in `src/styles.css`: Settings switches are now compact desktop controls (`42x24px` track with an `18x18px` thumb instead of the oversized `51x31px` treatment), while preserving the existing on/off semantics in Black Chrome and Platinum Silver.
- Fixed Usage activity contrast: the tooltip now uses an explicit dark surface with near-white text in both themes, and empty/future activity cells receive theme-specific visible surfaces so the calendar remains legible.
- Added a seventh regression assertion to `scripts/audit-regressions.mjs` covering switch dimensions, tooltip contrast, and theme-specific activity-cell states. Browser preview checks covered Settings, Usage, API in both themes; the native smoke pass additionally clicked a populated activity cell and observed `52.96M tokens on May 28` legibly.
- Verification: `npm.cmd run test:audit` passed 7/7; `npm.cmd run build` passed; `cargo test` passed 114/114 plus doctests; `npm.cmd audit --audit-level=moderate` reported 0 vulnerabilities; `git diff --check` passed; `npm.cmd run tauri build` produced optimized release, MSI, and NSIS bundles.
- Installed app: backed up the prior executable to `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260728-ui-polish`, replaced only the installed executable, and launched the new build. Release and installed SHA-256 both equal `A8AE0315DC15ADC5182A6A9B47F1CB5D02646A580CA4D480701265D4360B812D`; native process PID 32000 was alive and responding during the smoke pass.
- Live installed smoke test: both Black Chrome and Platinum Silver rendered the redesigned shell; Settings showed compact switches, Usage showed visible empty cells and a readable tooltip, and API retained the selected theme. The appearance was restored to Silver and the Codex tab after testing. No account/profile directory, login flow, gateway start, or gateway traffic was touched.

### 2026-07-29 - Repaired unknown Usage models and missing code-review cost
- Root causes: the incremental Usage cache kept aggregates for rollout files that had been deleted/archived, and some Codex rollouts emitted cumulative `token_count` before their first `turn_context`, so the leading usage was permanently assigned to `unknown`. `codex-auto-review` is a logical Codex mode without a direct LiteLLM price key.
- Backend fix: bumped the Usage cache version to 3 for a one-time clean rebuild; missing or shortened tracked JSONL files now invalidate cumulative cache state; cold Codex scans seed leading token deltas from the first declared model; `codex-auto-review` now resolves to `gpt-5.3-codex` for the existing estimated-USD pricing path, matching OpenAI's Code Review model mapping.
- TDD evidence: each new regression test failed against the old behavior, then passed after the minimal fix. Final Rust library suite passed 117/117, `cargo check --all-targets` passed, scoped rustfmt checks passed for `usage.rs` and `pricing.rs`, `npm.cmd run test:audit` passed 8/8, `npm.cmd run build` passed, and `git diff --check` passed. A broad `cargo fmt --check` still reports unrelated pre-existing formatting differences in untouched Rust modules; no broad formatting rewrite was applied.
- Production/live evidence: `npm.cmd run tauri build` produced the optimized executable plus MSI/NSIS. The prior installed executable was backed up at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260729-usage-pricing`; release and installed SHA-256 both equal `4F3A8A70953C43D867239C98F8CD9979234E9A693DE2CB8D0766ECC125D149CC`. The relaunched process PID 12184 was alive. Opening Usage in the installed app rebuilt the real cache to version 3: 0 `|unknown` buckets and the `codex-auto-review` row rendered an estimated `$0.80` for 825,418 tokens. No account/profile, login, gateway, or credential action was touched.

### 2026-07-29 - Cleared stale Codex login prompts and normalized Windows CLI paths
- Root cause and lifecycle fix: hidden Codex login readers emitted device-code prompts but never emitted a terminal lifecycle event, so success, failure, or cancellation could leave a global prompt visible. The backend now waits for the login process, drains both output readers, and emits an account-scoped `login-prompt-finished` event. Monotonic `attemptId` values prevent an older same-account or cross-account process, completion, or late output event from clearing/replacing the newest global prompt; spawn failures also emit a terminal event. The frontend additionally reconciles prompts against account state so a Ready/Active account cannot retain a stale banner.
- Display-only path fix: Windows CLI/config paths are normalized to backslashes and compacted from `C:\Users\<name>\...` to `~\...` in Settings summaries, read-only rows, and detected candidates. Raw values remain unchanged for selection, saving, and backend execution.
- TDD/review evidence: focused logic regressions cover same/different account merging, success/failure/cancel completion, superseded attempts, late output ordering, snapshot reconciliation, Windows/Unix shortening, immutability, and length limits. The original tests failed before the helpers existed, and the same-account overlap regression failed before attempt ordering was added. Final gates passed: `npm.cmd run test:logic` 2/2, `npm.cmd run test:audit` 10/10, `npm.cmd run build`, `cargo test` 118/118 plus 0 doctests, `cargo check --all-targets`, `npm.cmd audit --audit-level=moderate` with 0 vulnerabilities, and `git diff --check`. Clippy has no warnings in the touched `tools.rs`, `usage.rs`, or `pricing.rs`; 10 existing warnings remain in unrelated modules.
- Production/live evidence: `npm.cmd run tauri build` produced the optimized executable plus MSI/NSIS. The previous installed executable is backed up at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260729-login-lifecycle`; final release and installed SHA-256 both equal `D039477A48A7B16CB87D3BB115EAA9EBC0D6658222B2F063EBFE849D74D70A90`. The final installed process PID 18188 was responding with a nonzero window handle. Native inspection confirmed no Codex sign-in banner while accounts were Ready/Active; the immediately preceding attempt-order build also confirmed Settings rendered normalized `~\AppData\Roaming\npm\...` summaries plus normalized candidate paths, and the final rebuild changed only prompt ordering. Only navigation and the non-mutating path-review modal were exercised; no account, login, credential, profile, auto-switch, or gateway action was triggered.

### 2026-07-30 - Structured Codex device-code handoff

- Root cause: Windows device-code login was launched through the npm `cmd`/Node wrapper as a hidden process and inferred the code from terminal text. A live `needsLogin` profile had a running `codex login --device-auth` child but no app banner, so the browser's device page had no code to enter.
- Fix: Windows Codex device-code login now uses the CLI's `app-server --stdio` JSON-RPC auth flow. The app-server returns `verificationUrl` and `userCode` in a structured `chatgptDeviceCode` response; the app emits the existing account-scoped prompt event from those fields and waits for `account/login/completed`. Tokens remain owned and persisted by Codex; no credential is parsed, stored, or copied by the UI path.
- Verification: a real `app-server` initialize handshake passed through the same npm wrapper and pending profile without requesting a device code. New parser/completion tests were RED before implementation then GREEN. Final checks passed: `cargo test` 120/120, `cargo check --all-targets`, `npm.cmd run test:logic` 2/2, `npm.cmd run test:audit` 10/10, `npm.cmd run build`, and `git diff --check`. The pre-existing broad `rustfmt --check` differences outside this login change were left untouched.
- Production evidence: `npm.cmd run tauri build` completed successfully and produced MSI/NSIS bundles. Installed executable was backed up at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260730-device-code-structured` (SHA-256 `D039477A48A7B16CB87D3BB115EAA9EBC0D6658222B2F063EBFE849D74D70A90`). The new release and installed executable both hash to `09ED801BFA70DBBCA304EB41F40FFE37086607E55CCC4370AA8F600E9BCEEE05`; the relaunched native app process PID 2860 exposed the `AI Account Switcher` window. Existing Codex device-login children were not terminated or inspected.

### 2026-07-30 - App-server v1 handshake correction

- Follow-up diagnosis: the device-code profile showed `needsLogin` while the app-server child exited before a prompt. Its safe log metadata recorded exactly two processed RPC messages. Codex CLI `0.141.0` accepted `initialize`, but its v1 acknowledgement requires the empty notification `{ "method": "initialized" }`; the earlier `{ "method": "initialized", "params": {} }` closed the connection before `account/login/start`.
- Fix: send the version-compatible empty acknowledgement. A safe compatibility probe against the real configured `codex.cmd` and pending profile verified `initialize` plus the v1 acknowledgement followed by `account/read`, without generating a device code or accessing credentials.
- Verification: the new regression was RED before the helper existed, then GREEN. `cargo test` passed 121/121, `cargo check --all-targets`, `npm.cmd run test:logic` 2/2, `npm.cmd run test:audit` 10/10, `npm.cmd run build`, and `npm.cmd run tauri build` all passed. The release SHA-256 is `5EE844F94D444EBEEAF524C58130B01A853F485ED1D4A664C459AC9BA99AB78B`.
- Live status: Windows retained a lock on the installed executable after it closed, so it was not overwritten. The lock-safe rollback copy is `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260730-device-code-v1-handshake`. The release executable was started directly from `D:\Code\Code\ai-switcher\src-tauri\target\release\ai-account-switcher.exe` (PID 19536, native window confirmed) and uses the same app data. No device code, browser field, credential, or OAuth completion was automated.

### 2026-07-30 - Codex re-login state wire-format repair

- Root cause: Rust serializes `AccountState::NeedsLogin` with `#[serde(rename_all = "camelCase")]` as `needsLogin`, while the React contract and all UI comparisons expected `needs-login`. A pending Codex account therefore rendered as `Ready` and hid its `Re-login` action, even though the backend correctly marked it as needing authentication.
- Fix: aligned the frontend `AccountState` union, all account-state comparisons, lifecycle reconciliation, and the development fallback snapshot with the Rust `needsLogin` value. The card now renders `Signing in` and exposes `Re-login` for a non-default Codex profile such as `acc3`; the login itself remains an explicit user action.
- TDD and verification: the new frontend/Rust wire-contract test failed before the correction and then passed. Final checks passed: `npm.cmd run test:logic` 3/3, `npm.cmd run test:audit` 10/10, `npm.cmd run build`, `cargo test` 121/121, and `npm.cmd run tauri build` (optimized executable plus MSI and NSIS bundles). No account, device code, browser field, credential, or OAuth completion was automated.

### 2026-08-03 - Installed device-code fix deployed and live-verified

- Root cause of recurrence: Windows startup was launching the stale installed executable with SHA-256 `09ED801BFA70DBBCA304EB41F40FFE37086607E55CCC4370AA8F600E9BCEEE05`, rather than the repaired release executable. That old build was started with `--hidden` and retained both the pre-v1-handshake flow and pre-`needsLogin` frontend behavior.
- Deployment: stopped only the stale AI Account Switcher process after validating its exact path and hash. Preserved it at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260803-pre-device-code-fix`, then copied the verified release executable into the installed path. The installed file now hashes to `B8FFD764A4F202B5FDA39E59DB885A0B3CE3342CE36137A6C76ED900A7948D2B`.
- Live verification: restarted the installed executable as PID 1288; it responded with the native `AI Account Switcher` window. Native UI inspection confirmed the top device-code banner is present and `acc3` renders `Signing in` with a visible `Re-login` action. The code itself was not recorded, copied, entered, or otherwise used; account/profile/credential data was not changed.

### 2026-08-03 - Codex VS Code switch reload race fix

- Root cause: Codex subscription switching synchronized `~/.codex/auth.json`, forcibly terminated every matching `codex app-server`, and then sent VS Code `Developer: Reload Window` 1.2 seconds later. The extension log showed `stdin is destroyed` followed by repeated `Codex process is not available`; the new app-server could start, but the existing Codex webview remained in its fatal state.
- Fix: Codex switch plans still sync the selected credential and keep the automatic VS Code reload, but no longer pre-kill the app-server. VS Code's own reload now owns the graceful shutdown/restart sequence. The regression `codex_subscription_switch_reloads_the_ide_without_killing_its_app_server` was RED before the policy change and GREEN after it.
- Verification: `cargo test --lib` passed 121/121, `cargo check --all-targets`, `npm.cmd run test:logic` 3/3, `npm.cmd run test:audit` 10/10, `npm.cmd run build`, and `npm.cmd run tauri build` all passed. The installed executable was backed up at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260803-pre-vscode-race-fix`; release and installed SHA-256 both equal `E83C461CA89D1028200BB06C979ADBA556BFB03FE888E5FD97400935E02187DC`. The restarted installed process PID 31388 responded with the native window. No account switch, login, browser, credential, or OAuth action was automated during verification.

### 2026-08-03 - VS Code Codex webview Service Worker recovery

- Root cause: after VS Code reopened, the Codex panel failed before app-server initialization with `Could not register service worker`, leaving a black webview even though Codex runtime processes were present. This was a persistent VS Code webview cache state, separate from account credentials and the switcher runtime.
- Recovery: closed only the verified VS Code process tree, moved `%APPDATA%\Code\Service Worker` to the recoverable backup `C:\Users\HP\AppData\Roaming\Code\Service Worker.rollback-20260803-145843`, and reopened VS Code. The Service Worker directory was recreated automatically.
- Live verification: the new VS Code log session reported zero Service Worker registration errors and zero `Codex process is not available` lines; VS Code remained responsive. No workspace files, account state, credentials, or login/browser flows were changed.

### 2026-08-03 - Verified VS Code reload delivery after Codex account switch

- Root cause of the remaining frozen panel: credential synchronization was correct (the selected profile and machine-default `auth.json` SHA-256 values matched), but the fire-and-forget Command Palette automation did not actually reload VS Code. The switch stamp was written at 15:00:43 while the original extension-host process and Codex log session remained unchanged. The three-minute throttle could also suppress a legitimate follow-up account switch.
- Fix: coalesce only duplicate events within ten seconds; open the Command Palette with F1; allow the search result to settle; verify that VS Code created a new child process; and retry up to three times when the palette misses. A result marker now records `reloaded`, `reload failed`, `skipped duplicate`, or `no VS Code window` for safe follow-up diagnosis. The earlier no-pre-kill policy remains unchanged.
- TDD and verification: the new reload-delivery regression was compile-time RED before `codex_frontend_refresh_script` existed and GREEN after the implementation. The embedded PowerShell parsed without errors. `cargo test --lib` passed 122/122, `cargo check --all-targets`, `npm.cmd run test:logic` 3/3, `npm.cmd run test:audit` 10/10, `npm.cmd run build`, `git diff --check` for the touched paths, and `npm.cmd run tauri build` all passed. Broad `cargo fmt --all --check` still reports pre-existing formatting differences in unrelated and previously modified Rust code, so no broad formatting rewrite was applied.
- Deployment: the previous installed executable was preserved at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260803-151611-vscode-reload-confirm`. Release and installed SHA-256 both equal `78E1351DDF23E7302E3275826286BAEFD4E9D80F4662F86A518B21D4BB267838`; the installed app restarted as PID 15808 and responded with a nonzero native window handle. No additional account switch, login, credential mutation, or OAuth action was automated for the final live gate.
- Current-session recovery: gracefully closed the already-frozen VS Code window without a force-stop and reopened the same restored workspace. The new VS Code window PID 11836 was responsive; its fresh Codex log recorded one extension activation, one app-server spawn, authenticated account present, zero fatal/runtime-unavailable lines, and zero Service Worker registration errors. A real post-fix account switch remains intentionally unperformed because it would mutate the user's active account.

### 2026-08-03 - VS Code reload command made Vietnamese-IME safe

- Confirmed root cause: the reload helper typed `Developer: Reload Window` as simulated keystrokes. With a Vietnamese IME active, the `w` sequence was transformed so `Window` became `ưindow`; VS Code then ranked `Developer: Reload Webviews` first and Enter repeatedly reloaded only the webview instead of the window/extension host.
- Fix: the helper now places the exact ASCII command on the clipboard temporarily and pastes it with Ctrl+V, bypassing keyboard-layout and IME translation. It snapshots and restores the previous multi-format clipboard value, and does not overwrite a newer clipboard value if the user copied something during the short paste interval. Clipboard access retries three times if another process briefly owns it. The existing foreground guard, real-reload verification, and three-attempt retry remain intact.
- TDD and verification: the IME regression failed against the direct-typing implementation and passed after the clipboard path was added. Static inspection found zero direct typed-command calls and exactly one clipboard paste; the embedded PowerShell parsed without errors. `cargo test --lib` passed 123/123, `cargo check --all-targets`, `npm.cmd run test:logic` 3/3, `npm.cmd run test:audit` 10/10, `npm.cmd run build`, and `npm.cmd run tauri build` all passed.
- Deployment: preserved the previous installed executable at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260803-153145-vscode-ime-fix`. Release and installed SHA-256 both equal `AAB1416C7440384891D05D899FCB2E5720D8BA6B3F7C696A7579F95606604BE7`; the installed app restarted as PID 29368 and responded with a nonzero native window handle. No account switch, login, credential, or OAuth action was automated for the final live gate.

### 2026-08-03 - Secure credential vault implementation started

- Scope: reviewed the existing React/Tauri account, login, store, snapshot, and delete flows before adding a separate encrypted vault. OAuth/device-code behavior and profile credential isolation remain unchanged; credentials are entered explicitly by the user and are never scraped from a browser or login process.
- Baseline before changes: `cargo test` passed 123/123, `npm.cmd run test:audit` passed 10/10, `npm.cmd run test:logic` passed 3/3, `npm.cmd run build` passed, and `git diff --check` passed.
- Security design: vault records will be encrypted at rest with a user-entered vault passkey, kept out of `state.json`, logs, and account snapshots; reads require an unlocked in-memory session and account deletion will remove the matching vault record.

### 2026-08-03 - Encrypted credential vault installed

- Verification before install: `cargo test --quiet` passed 126/126, `cargo check --all-targets`, `npm.cmd run test:audit` 11/11, `npm.cmd run test:logic` 3/3, `npm.cmd run build`, `git diff --check`, and `npm.cmd run tauri build -- --debug` all passed.
- Deployment: the NSIS installer `AI Account Switcher_0.5.5_x64-setup.exe` completed with exit code 0. The installed executable at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe` is version 0.5.5, has the new 32 MB build size, and launched successfully with `Responding = True` from the installed path.
- No account, login, browser, OAuth, API gateway, or credential action was automated during installation verification. The source remains committed at `c1edb17`.

### 2026-08-03 - Replaced debug console build with release GUI build

- Root cause: the installed bundle had been produced with `npm.cmd run tauri build -- --debug`. `src-tauri/src/main.rs` selects the Windows GUI subsystem only when `debug_assertions` is disabled, so that debug artifact correctly identified itself as `Windows Console` and opened a terminal alongside the app.
- Fix and verification: `npm.cmd run tauri build` produced the optimized release executable plus MSI/NSIS bundles. The release NSIS installer completed with exit code 0. Direct PE-header inspection reports subsystem `2` (`Windows GUI`) for both the release and installed executables; the preserved debug executable reports subsystem `3` (`Windows Console`). The installed version 0.5.5 relaunched from `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe` as PID 8744 with `Responding = True` and a nonzero `AI Account Switcher` main window.
- Recovery: the superseded debug executable remains available at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260803-debug-console`. No account, login, OAuth, profile, gateway, or vault record was changed during the replacement.

### 2026-08-04 - Restored Codex desktop pet pointer input

- Root cause: the Codex desktop pet is a separate `Chrome_WidgetWin_1` overlay window. On the running Windows build it carried `WS_EX_TRANSPARENT` (`0x2800A8`), so `WindowFromPoint` returned the Codex window underneath instead of the pet child window; both visible pet controls were therefore click-through. The AI Account Switcher source had no pet component, so no account UI code was changed.
- Fix: added a Windows-only, process-path/title/class/size guarded repair. It clears only `WS_EX_TRANSPARENT` from the small Codex pet window and preserves topmost, tool-window, position, activation, and all account/profile/auth/reload behavior. The repair runs once at startup, every 10 seconds while the app is alive, and for a bounded retry window after the existing Codex frontend restart.
- TDD/verification: the new pet-window contract was intentionally RED before implementation (unresolved repair helpers), then GREEN. Final Rust library suite passed 131/131; `cargo check --all-targets`, `npm.cmd run test:logic` 3/3, `npm.cmd run test:audit` 11/11, `npm.cmd run build`, `git diff --check`, and `npm.cmd run tauri build` all passed.
- Live proof: a controlled regression re-added `WS_EX_TRANSPARENT` to the existing pet without closing Codex; hit-testing first returned an underlying window, then the installed AI Switcher monitor cleared the flag within the 12-second observation window and hit-testing returned the pet root. The installed release process PID 16180 was responding. Release and installed SHA-256 both equal `6BCF7A845E5DB53D8A7AE95460B3F1BC7A49DAE8C1A8DD1AD35D6BEE5A1362B4`; the prior executable is preserved at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260804-153439-codex-pet-fix`. No account switch, login, credential, profile, gateway, or OAuth action was automated.

### 2026-08-04 - Kept Codex pet controls inside a stable hit-test area

- Deeper root cause: Codex's pet renderer draws the sprite/control row beyond a native overlay client area that it repeatedly returns to roughly 400–416px. A one-shot 520px resize therefore reverted within about 50–65ms; the visible lower button could remain outside the top-level HWND even after `WS_EX_TRANSPARENT` was cleared.
- Fix: the Windows repair now discovers only the verified Codex pet every 500ms, then maintains that exact HWND/PID at a lightweight 10ms heartbeat. The heartbeat clears only the transparent hit-test bit, keeps a 520px client height while the pet exists, and moves edge-clipped layouts to the deterministic top rail; it never changes account/profile/auth/reload state. Stale HWNDs are dropped when the process/window changes.
- Verification: `cargo test --lib` passed 135/135, `cargo check --all-targets`, `npm.cmd run test:logic` 3/3, `npm.cmd run test:audit` 11/11, `npm.cmd run build`, `git diff --check`, and `npm.cmd run tauri build` all passed. Controlled Codex-window probes confirmed that a 5–10ms targeted heartbeat holds the 520px client area and that the top-rail position keeps the sprite/control composition on-screen; no click, account switch, login, credential, gateway, or OAuth action was automated.
- Deployment: the installed release at `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe` was replaced only after preserving `C:\Users\HP\AppData\Local\AI Account Switcher\ai-account-switcher.exe.rollback-20260804-164956-codex-pet-targeted-heartbeat`. Release and installed SHA-256 both equal `DC50E77DB528F2D6F2556AB3B0BC1CA902157228BF910108076E024DB9AC3083`; installed PID 10344 was responding. The Codex pet was not visible in the final post-install snapshot, so no direct click was attempted.
