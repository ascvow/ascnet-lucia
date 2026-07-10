import { createInterface } from "node:readline";

/** 向 stdout 写入一条 MCP JSON-RPC 消息。 */
function send(message: unknown): void {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

/** 处理测试需要的最小 MCP 方法集合。 */
function handle(message: Record<string, unknown>): void {
  const id = message.id;
  const method = message.method;
  if (id === undefined) return;

  if (method === "initialize") {
    send({
      jsonrpc: "2.0",
      id,
      result: {
        capabilities: { tools: {} },
        protocolVersion: "2024-11-05",
        serverInfo: { name: "mock-mcp", version: "1.0.0" },
      },
    });
    return;
  }

  if (method === "tools/list") {
    send({
      jsonrpc: "2.0",
      id,
      result: {
        tools: [
          {
            name: "get_design_node",
            description: "读取测试原型中的设计节点。",
            inputSchema: {
              type: "object",
              properties: { nodeId: { type: "string" } },
              required: ["nodeId"],
            },
          },
        ],
      },
    });
    return;
  }

  if (method === "tools/call") {
    const params = message.params as {
      name?: string;
      arguments?: Record<string, unknown>;
    };
    send({
      jsonrpc: "2.0",
      id,
      result: {
        content: [
          {
            type: "text",
            text: JSON.stringify({
              source: "mock-mcp",
              tool: params?.name,
              arguments: params?.arguments,
            }),
          },
        ],
        isError: false,
      },
    });
    return;
  }

  send({
    jsonrpc: "2.0",
    id,
    error: { code: -32601, message: `未知方法：${String(method)}` },
  });
}

const input = createInterface({ input: process.stdin });
input.on("line", (line) => {
  try {
    handle(JSON.parse(line) as Record<string, unknown>);
  } catch (error) {
    process.stderr.write(`解析测试 MCP 请求失败：${String(error)}\n`);
  }
});
