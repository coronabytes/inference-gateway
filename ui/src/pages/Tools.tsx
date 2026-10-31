import { Tabs, useHashTab } from "../components/controls";
import { A2aAgents } from "./A2aAgents";
import { McpServers } from "./McpServers";
import { ToolRouters } from "./ToolRouters";

const TABS = ["mcp", "routers", "agents"] as const;

/** What agents reach through the gateway besides models: MCP servers, the tool routers that pick
 *  among their tools, and A2A agents. */
export function Tools() {
  const [tab, setTab] = useHashTab("tools", TABS);
  return (
    <div className="page">
      <Tabs
        label="Tools"
        options={[
          { value: "mcp", label: "MCP servers" },
          { value: "routers", label: "Tool routers" },
          { value: "agents", label: "A2A agents" },
        ]}
        value={tab}
        onChange={setTab}
      />
      {tab === "mcp" ? <McpServers /> : tab === "routers" ? <ToolRouters /> : <A2aAgents />}
    </div>
  );
}
