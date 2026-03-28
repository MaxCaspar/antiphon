#!/usr/bin/env bash
set -euo pipefail

cat <<'JSON'
{"type":"thread.started","thread_id":"thread-interleaved-1"}
{"type":"turn.started"}
{"type":"response.reasoning_summary_text.delta","delta":"think-a"}
{"type":"item.started","item":{"id":"tool_1","type":"command_execution","status":"in_progress","command":"bash -lc 'echo hi'"}}
{"type":"response.output_item.added","item":{"type":"reasoning","summary":[{"type":"summary_text","text":"think-b"}]}}
{"type":"item.completed","item":{"id":"tool_1","type":"command_execution","status":"completed","command":"bash -lc 'echo hi'","exit_code":0}}
{"type":"response.reasoning.delta","item":{"id":"item_1"},"delta":{"text":"think-c"}}
{"type":"item.completed","item":{"id":"msg_1","type":"agent_message","text":"interleaved stream done"}}
{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":10}}
JSON
