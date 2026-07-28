import { describe, expect, it } from "vitest";
import { connectorSteps, jobTemplates } from "./scenarios";

describe("jobTemplates", () => {
  it("defines the payment workflow and change reaction job templates", () => {
    expect(jobTemplates.map((t) => t.id)).toEqual([
      "inbound-payment-workflow",
      "order-change-reaction",
    ]);

    expect(jobTemplates[0].title).toContain("payment");
    expect(jobTemplates[1].title).toContain("change");
  });

  it("keeps the payment workflow and change reaction step chains intact", () => {
    const [payment, reaction] = jobTemplates;

    expect(connectorSteps(payment).map((step) => step.id)).toEqual([
      "ingest-bank-file",
      "transform-to-bank-format",
      "dispatch-to-settlement-queue",
      "platform-health-gate",
    ]);
    expect(connectorSteps(reaction).map((step) => step.id)).toEqual([
      "capture-change-feed",
      "persist-change-snapshot",
      "publish-drasi-signal",
      "platform-health-gate",
    ]);
  });

  it("ships template steps as real connector execution payloads", () => {
    for (const template of jobTemplates) {
      for (const step of connectorSteps(template)) {
        expect("simulate" in step).toBe(false);
      }
    }
  });

  it("keeps replay targets tied to defined step ids", () => {
    for (const template of jobTemplates) {
      const ids = new Set(connectorSteps(template).map((step) => step.id));
      for (const replayId of template.replayStepIds) {
        expect(ids.has(replayId)).toBe(true);
      }
    }
  });
});
