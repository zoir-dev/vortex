<script setup lang="ts">
// Shell: a left icon rail (AppShell) + the routed page (pages/home, settings,
// contacts, …) in the content area. The clipboard-history POPUP window loads
// the same frontend at /#/clipboard — it renders bare (no rail, no nav).
import { computed, onMounted, onUnmounted, ref } from "vue";
import { useRoute } from "vue-router";
import AppShell from "@/components/AppShell.vue";
import OnboardingFlow from "@/pages/onboarding/OnboardingFlow.vue";
import { introDone } from "@/lib/intro";
import { onFatal, onPeerStoreError } from "@/lib/bridge";
import type { UnlistenFn } from "@tauri-apps/api/event";

const route = useRoute();
const bare = computed(() => route.path.startsWith("/clipboard"));

// Suppress the webview's native right-click menu (Reload / Inspect / Back …) —
// a desktop app shouldn't expose it, and an accidental "Reload" there blanked
// the connection state. Components that want a real right-click menu attach
// their own `@contextmenu.prevent` handler (e.g. the device cards' "Forget"):
// those still run, since this only cancels the DEFAULT menu, never their logic.
// Text fields and copyable content (.selectable-text, e.g. SMS bubbles) keep
// the native menu so copy / paste stays available.
function onContextMenu(e: MouseEvent) {
  const el = e.target as HTMLElement | null;
  if (el?.closest('input, textarea, [contenteditable=""], [contenteditable="true"], .selectable-text')) return;
  e.preventDefault();
}
// Health banner. The backend has always emitted `vortex:fatal` and
// `vortex:peer_store_error`, and until now nothing anywhere listened — so when
// the worker could not reach secure storage or Bluetooth, the app carried on
// looking completely normal with a phone that simply read "Offline", and the
// only clue was a line in the journal. The peer-store event exists precisely so
// the UI does NOT show the "pair your phone" prompt on an empty-because-locked
// store, which would create a duplicate trust entry.
const health = ref("");
let stopFatal: UnlistenFn | null = null;
let stopPeerErr: UnlistenFn | null = null;

onMounted(async () => {
  window.addEventListener("contextmenu", onContextMenu);
  stopFatal = await onFatal(msg => { health.value = msg; });
  stopPeerErr = await onPeerStoreError(msg => { health.value = msg; });
});
onUnmounted(() => {
  window.removeEventListener("contextmenu", onContextMenu);
  stopFatal?.();
  stopPeerErr?.();
});
</script>

<template>
  <!-- The clipboard popup window renders bare; the main window shows the
       first-run intro until it's completed, then the app shell. -->
  <RouterView v-if="bare" />
  <template v-else>
    <!-- Shown above everything: if this is up, the parts of Vortex that talk
         to the phone are not running, and no other screen can say so. -->
    <div v-if="health" class="health-banner" role="alert">
      <span class="health-banner__dot" aria-hidden="true"></span>
      <span class="health-banner__text">{{ health }}</span>
      <button class="health-banner__close" title="Dismiss" @click="health = ''">×</button>
    </div>
    <OnboardingFlow v-if="!introDone" />
    <AppShell v-else />
  </template>
</template>

<style scoped>
.health-banner {
  display: flex;
  align-items: center;
  gap: 0.6rem;
  padding: 0.55rem 0.9rem;
  background: #7f1d1d;
  color: #fee2e2;
  font-size: 0.82rem;
  line-height: 1.4;
}
.health-banner__dot {
  width: 0.5rem;
  height: 0.5rem;
  border-radius: 50%;
  background: #fca5a5;
  flex: none;
}
.health-banner__text { flex: 1; }
.health-banner__close {
  border: 0;
  background: transparent;
  color: inherit;
  font-size: 1.1rem;
  line-height: 1;
  cursor: pointer;
  padding: 0 0.2rem;
}
.health-banner__close:hover { color: #fff; }
</style>
