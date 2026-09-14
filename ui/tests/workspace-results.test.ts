import { afterEach, expect, test } from "bun:test";
import { __registerToast } from "../src/utils/notify";
import { reportWorkspaceResult } from "../src/utils/results";

afterEach(() => __registerToast(null));

test("an incomplete operation with no service errors still needs attention", () => {
  let kind = "";
  let message = "";
  __registerToast((nextKind, nextMessage) => { kind = nextKind; message = nextMessage; });
  reportWorkspaceResult("Create", {operation: {status: "needs_attention", phase: "post-create"}});
  expect(kind).toBe("warning");
  expect(message).toContain("post-create");
});

test("partial removal never produces a success toast", () => {
  let kind = "";
  __registerToast((nextKind) => { kind = nextKind; });
  reportWorkspaceResult("Remove", {vcs_ref_deleted: false});
  expect(kind).toBe("error");
});

test("skipped dependent hooks show the actual reason", () => {
  let message = "";
  __registerToast((_kind, nextMessage) => { message = nextMessage; });
  reportWorkspaceResult("Switch", {hooks: [{phase: "post-create", succeeded: 0, failed: 0, skipped: 1, background: 0, errors: [], skipped_reasons: ["required service failed"]}]});
  expect(message).toContain("required service failed");
  expect(message).not.toContain("needs approval");
});


test("optional hook failures produce warnings without failing a ready workspace", () => {
  let kind = "";
  __registerToast((nextKind) => { kind = nextKind; });
  reportWorkspaceResult("Switch", {operation: {status: "ready", phase: "done"}, hooks: [{phase: "post-create", succeeded: 0, failed: 1, required_failed: 0, skipped: 0, background: 0, errors: ["optional lint failed"]}]});
  expect(kind).toBe("warning");
});
