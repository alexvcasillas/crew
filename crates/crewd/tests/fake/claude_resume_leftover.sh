#!/bin/sh
# Like a resumed session that had a background task left over: a task
# notice and an empty result with no turns first, then the real turn,
# which takes longer than the wait after a turn ends.
S='"session_id":"00000000-0000-4000-8000-000000000002"'
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"t1\",\"status\":\"stopped\",\"summary\":\"Background shell command didn't finish before the previous session ended\",$S}"
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"cwd\":\"/work/demo\",\"model\":\"claude-haiku-4-5\",\"tools\":[],$S}"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"\",\"duration_ms\":29,\"num_turns\":0,\"permission_denials\":[],$S}"
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"cwd\":\"/work/demo\",\"model\":\"claude-haiku-4-5\",\"tools\":[],$S}"
sleep 4
printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"It is merged.\"}]},$S}"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"It is merged.\",\"duration_ms\":4000,\"num_turns\":2,\"permission_denials\":[],$S}"
