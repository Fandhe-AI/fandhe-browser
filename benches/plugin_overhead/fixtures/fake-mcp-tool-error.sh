#!/bin/sh
# テスト専用の偽 MCP プラグイン（TASK-95.3）。initialize には成功し、tools/call には isError: true を返す。
read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}'
read line
read line
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"isError":true,"content":[{"type":"text","text":"fail"}]}}'
sleep 0.3
exit 0
