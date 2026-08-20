// SPDX-License-Identifier: AGPL-3.0-or-later
import "../styles.css";
import { mount } from "svelte";
import App from "./App.svelte";
import AuthGate from "../lib/AuthGate.svelte";

const target = document.getElementById("app");
if (!target) throw new Error("missing #app mount target");

export default mount(AuthGate, { target, props: { app: App } });
