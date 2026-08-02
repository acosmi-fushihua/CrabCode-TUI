import type { z } from "zod/v4";
import type { ContentBlockParam } from "./api-types.js";
import type { AssistantMessage } from "./message.js";
import type { AnyObject, Tool, ToolUseContext } from "../Tool.js";
import type { PermissionDecision } from "../utils/permissions/PermissionResult.js";
import type { PermissionUpdate } from "../utils/permissions/PermissionUpdateSchema.js";

export type WorkerBadge = {
  name: string;
  color: string;
};

export type ToolUseConfirm<Input extends AnyObject = AnyObject> = {
  assistantMessage: AssistantMessage;
  tool: Tool<Input>;
  description: string;
  input: z.infer<Input>;
  toolUseContext: ToolUseContext;
  toolUseID: string;
  permissionResult: PermissionDecision;
  permissionPromptStartTimeMs: number;
  classifierCheckInProgress?: boolean;
  classifierAutoApproved?: boolean;
  classifierMatchedRule?: string;
  workerBadge?: WorkerBadge;
  onUserInteraction(): void;
  onAbort(): void;
  onDismissCheckmark?(): void;
  onAllow(
    updatedInput: z.infer<Input>,
    permissionUpdates: PermissionUpdate[],
    feedback?: string,
    contentBlocks?: ContentBlockParam[],
  ): void;
  onReject(feedback?: string, contentBlocks?: ContentBlockParam[]): void;
  recheckPermission(): Promise<void>;
};
