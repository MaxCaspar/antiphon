#!/usr/bin/env bash
set -euo pipefail

cat <<'JSON'
{"type":"thread.started","thread_id":"thread-tool-1"}
{"type":"item.started","item":{"id":"tool_1","type":"command_execution","status":"in_progress","command":"bash -lc 'echo hi'"}}
{"type":"item.completed","item":{"id":"tool_1","type":"command_execution","status":"completed","command":"bash -lc 'echo hi'","exit_code":0}}
{"type":"item.completed","item":{"id":"msg_1","type":"agent_message","text":"tool stream done"}}
JSON
