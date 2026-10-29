import { createContext, useContext } from "react";
import type { SessionInfo } from "./api";

/** The signed-in session, for pages that show users less than admins. */
export const SessionContext = createContext<SessionInfo | null>(null);

/**
 * Whether the signed-in person is a user rather than an admin: they see only the projects an
 * admin added them to, and there manage keys and see the playground, request log and metrics.
 * The gateway enforces this; the console only leaves out what they could not use.
 */
export function useIsUser(): boolean {
  return useContext(SessionContext)?.role === "user";
}
