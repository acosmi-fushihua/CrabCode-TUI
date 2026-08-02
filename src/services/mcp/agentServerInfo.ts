export interface AgentMcpServerInfo {
  name: string;
  transport: "stdio" | "sse" | "http" | "ws";
  command?: string;
  url?: string;
  needsAuth: boolean;
  sourceAgents: string[];
  isAuthenticated?: boolean;
}
