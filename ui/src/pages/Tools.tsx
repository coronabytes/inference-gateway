import { Tabs, useHashTab } from "../components/controls";
import { A2aAgents } from "./A2aAgents";
import { McpServers } from "./McpServers";

const TABS = ["mcp", "agents"] as const;

/** What agents reach through the gateway besides models: MCP servers and A2A agents. */
export function Tools() {
  const [tab, setTab] = useHashTab("tools", TABS);
  return (
    <div className="page">
      <Tabs
        label="Tools"
        options={[
          { value: "mcp", label: "MCP servers" },
          { value: "agents", label: "A2A agents" },
        ]}
        value={tab}
        onChange={setTab}
      />
      {tab === "mcp" ? <McpServers /> : <A2aAgents />}
    </div>
  );
}
