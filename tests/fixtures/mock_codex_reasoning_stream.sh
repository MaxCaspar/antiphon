#!/usr/bin/env bash
set -euo pipefail

echo '{"type":"thread.started","thread_id":"thread-reasoning-1"}'
echo '{"type":"turn.started"}'
echo '{"type":"response.reasoning_summary_text.delta","delta":"think-a"}'
echo '{"type":"response.output_item.added","item":{"type":"reasoning","summary":[{"type":"summary_text","text":"think-b"}]}}'
echo '{"type":"response.reasoning.delta","item":{"id":"item_1"},"delta":{"text":"think-c"}}'
echo '{"type":"response.trace.delta","item":{"id":"item_2"},"delta":{"text":"ignore-me"}}'
echo '{"type":"response.output_text.delta","delta":{"text":"assistant token"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":10}}'
