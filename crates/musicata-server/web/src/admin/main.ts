import "../styles.css";
import { mount } from "svelte";
import App from "./App.svelte";
import AuthGate from "../lib/AuthGate.svelte";

const target = document.getElementById("app");
if (!target) throw new Error("missing #app mount target");

// The admin page is gated to administrators.
export default mount(AuthGate, { target, props: { app: App, requireAdmin: true } });
