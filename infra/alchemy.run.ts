// SBB commute stack: one Rust (workers-rs -> wasm) Cloudflare Worker.
//
//   bun run login     once, to authenticate
//   bun run deploy    builds ../worker with worker-build, then deploys
//   bun run destroy
//
// Secrets come from ../.env (symlinked here as .env; Bun loads it).
import * as Alchemy from "alchemy";
import * as Cloudflare from "@alchemy.run/cloudflare";
import * as Effect from "effect/Effect";
import * as Redacted from "effect/Redacted";

const required = (name: string) => {
  const v = process.env[name];
  if (!v) throw new Error(`${name} is missing from .env`);
  return v;
};

export default Alchemy.Stack(
  "SbbCommute",
  { providers: Cloudflare.providers(), state: Cloudflare.state() },
  Effect.gen(function* () {
    const stage = yield* Alchemy.Stage;
    const worker = yield* Cloudflare.Worker("Api", {
      name: stage === "prod" ? "sbb-commute" : `sbb-commute-${stage}`,
      main: "../worker/build/index.js",
      compatibility: { date: "2026-11-01" },
      workersDev: true,
      env: {
        DESTINATION: "Zürich Oerlikon",
        // Morning window the e-paper board shows (Europe/Zurich local time).
        WINDOW_START: "06:30",
        WINDOW_END: "09:00",
        SYNC_LEAD_MIN: "30",
        ACTIVE_DAYS: "Mon,Tue,Wed,Thu,Fri",
        HOME_ADDRESS: Redacted.make(required("HOME_ADDRESS")),
        HOME_LAT: Redacted.make(required("HOME_LAT")),
        HOME_LON: Redacted.make(required("HOME_LON")),
        TOKEN: Redacted.make(required("WORKER_TOKEN")),
        // Second trip, shown until its latest good departure has passed.
        TRIP2_LABEL: "Zug",
        TRIP2_ARRIVE_BY: "08:00",
        // She takes Tram 7 from Ueberlandpark, a 7 min walk from home.
        TRIP2_FROM: "Zürich, Ueberlandpark",
        TRIP2_WALK_MIN: "7",
        TRIP2_TO: Redacted.make(required("TRIP2_TO")),
      },
    });
    return { url: worker.url };
  }),
);
