// SPDX-License-Identifier: AGPL-3.0-or-later
import "../styles.css";
import { mount } from "svelte";
import App from "../player/App.svelte";
import AuthGate from "../lib/AuthGate.svelte";

const target = document.getElementById("app");
if (!target) throw new Error("missing #app mount target");

// Compatible /admin entry, with the same player/audio lifecycle and an admin gate.
export default mount(AuthGate, { target, props: { app: App, requireAdmin: true } });
