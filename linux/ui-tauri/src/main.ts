import { createApp } from "vue";
import "./style.css";
import App from "./App.vue";
import { i18n } from "@/lib/i18n";
import { router } from "@/router";
import { initConnectionStore } from "@/lib/connectionStore";
import { initHome } from "@/composables/useHome";
import { initContacts } from "@/composables/useContacts";
import { initRecents } from "@/composables/useRecents";
import { initMessages } from "@/composables/useMessages";
import "@/lib/theme"; // initialise theme (side-effect)
import { initWindowFit, endWindowFit } from "@/lib/fitWindow";

// Subscribe to peer/identity state + home logic once, app-wide, so it persists
// across route changes (no "offline" flash / no re-subscribe on remount).
initConnectionStore();
initHome();
initContacts();
initRecents();
initMessages();

createApp(App).use(i18n).use(router).mount("#app");

// The window is still hidden at this point (tauri.conf `visible: false`).
// Measure the laid-out UI and hand Rust the size to open at, so the first
// frame the user sees already fits its own buttons.
initWindowFit();
// The landing page decides the window size; the first navigation away from it
// ends the sizing. Wired to the router because vue-router's hash mode uses
// pushState, so no DOM event reports an in-app route change.
//
// `from.matched.length` is what separates a real navigation from the initial
// one: vue-router fires `afterEach` for the first route resolution as well, and
// treating that as "the user navigated" stopped the fit before it had measured
// anything at all — the window stayed at its configured size with not one fit
// in the log.
router.afterEach((_to, from) => {
  if (from.matched.length > 0) endWindowFit();
});
