<script setup lang="ts">
// The "My devices" dashboard — a calm control centre with three cards (this
// laptop, the phone, the earbuds), translated from the Vortex design system.
// Phone + earbuds are wired to live daemon state; the laptop card's battery and
// mirror-to-phone action are follow-ups (no UI accessor yet).
import { ref, computed } from "vue";
import { useI18n } from "vue-i18n";
import { invoke } from "@tauri-apps/api/core";
import {
  Laptop,
  Smartphone,
  Headphones,
  Video,
  MonitorUp,
  Loader2,
  Plus,
  BellRing,
  SwitchCamera,
  TabletSmartphone,
} from "lucide-vue-next";
import {
  activeEarbuds,
  batteryClass,
  batteryIcon,
  earbudsMenuOpen,
  forgetTarget,
  isSwitching,
  mirrorStarting,
  mirrorActive,
  openEarbudsPicker,
  openPairPhoneModal,
  phoneOnline,
  phoneConnecting,
  primaryPeer,
  primaryPeerState,
  startMirror,
  peerSwitchScanning,
  peerSwitchCandidates,
  peerSwitchNoneFound,
  startPeerSwitch,
  abortPeerSwitch,
  choosePeer,
} from "@/composables/useHome";
import { peers } from "@/lib/connectionStore";

const { t } = useI18n();

const connectedCount = computed(
  () => 1 + (phoneOnline.value ? 1 : 0) + (activeEarbuds.value?.connected ? 1 : 0),
);

// Continuity Camera: use the phone as a laptop webcam (v4l2 pipe).
const cameraOn = ref(false);
async function toggleCamera() {
  cameraOn.value = !cameraOn.value;
  try {
    await invoke("set_camera_request", { on: cameraOn.value });
  } catch {
    cameraOn.value = !cameraOn.value; // revert on failure
  }
}

// Which lens the phone opens. The phone has honoured `camera_facing` from the
// start and the strings for this button were already written; nothing ever
// called the command, so every session was stuck on the default front lens.
// Shown only while the camera is on — there is nothing to flip otherwise.
const cameraFacing = ref<"front" | "back">("front");
async function flipCamera() {
  const next = cameraFacing.value === "front" ? "back" : "front";
  cameraFacing.value = next;
  try {
    await invoke("set_camera_facing", { facing: next });
  } catch (e) {
    cameraFacing.value = next === "front" ? "back" : "front"; // revert
    console.warn("set_camera_facing failed", e);
  }
}

// Find-My: ring the phone loudly (even on silent) to locate it. The button
// pulses for a moment as feedback; the phone keeps ringing ~25s or until the
// user taps Stop on it.
const ringing = ref(false);
let ringTimer: ReturnType<typeof setTimeout> | undefined;
async function ringPhone() {
  try {
    await invoke("ring_phone");
    ringing.value = true;
    clearTimeout(ringTimer);
    ringTimer = setTimeout(() => (ringing.value = false), 2500);
  } catch {
    /* ignore — offline phone simply won't get the heartbeat */
  }
}

const earbudsStatus = computed(() => {
  if (!activeEarbuds.value) return t("earbuds.not_connected");
  return activeEarbuds.value.on === "local" ? t("earbuds.on_local") : t("earbuds.on_peer");
});
</script>

<template>
  <div class="flex flex-col gap-[22px] px-8 py-7">
    <!-- Header -->
    <header>
      <h1 class="text-2xl font-semibold tracking-[-0.5px]">{{ t("nav.home") }}</h1>
      <p class="mt-0.5 text-[13.5px] text-muted-foreground">
        {{ t("home.synced", { count: connectedCount }) }}
      </p>
    </header>

    <!-- Device grid -->
    <div class="grid grid-cols-2 gap-4">
      <!-- THIS LAPTOP (full width) -->
      <div class="vx-card col-span-2 flex items-center gap-4">
        <span class="vx-icon"><Laptop class="h-6 w-6" /></span>
        <div class="shrink-0">
          <div class="flex items-center gap-2.5">
            <span class="text-base font-semibold">{{ t("device.this") }}</span>
            <span
              class="rounded-full bg-primary/[0.12] px-2 py-[3px] text-[10px] font-semibold uppercase tracking-[0.6px] text-primary"
            >{{ t("device.this") }}</span>
          </div>
          <div class="mt-1.5 flex items-center gap-2">
            <span class="vx-dot bg-primary shadow-[0_0_8px_hsl(var(--primary)/0.6)]" />
            <span class="text-[12.5px] text-muted-foreground">{{ t("device.linux") }}</span>
          </div>
        </div>
      </div>

      <!-- PHONE -->
      <button
        v-if="!primaryPeer"
        class="vx-card flex flex-col items-center justify-center gap-2 py-7 text-center hover:border-primary/40"
        @click="openPairPhoneModal"
      >
        <span class="vx-icon"><Plus class="h-5 w-5" /></span>
        <span class="text-sm font-medium">{{ t("pair.add_phone") }}</span>
        <span class="text-xs text-muted-foreground">{{ t("pair.add_phone_hint") }}</span>
      </button>

      <div
        v-else
        class="vx-card flex flex-col gap-3.5"
        @contextmenu.prevent="forgetTarget = primaryPeer"
      >
        <div class="flex items-center gap-3">
          <span class="vx-icon"><Smartphone class="h-[22px] w-[22px]" /></span>
          <div class="min-w-0 flex-1">
            <div class="text-[15px] font-semibold">
              {{ primaryPeerState?.name || primaryPeer.peer_name || t("device.android") }}
            </div>
            <div class="mt-px text-xs text-muted-foreground">{{ t("device.android") }}</div>
          </div>
          <!-- Find-My: ring the phone to locate it (only when it's reachable). -->
          <button
            v-if="phoneOnline"
            class="vx-ring"
            :class="{ 'vx-ring--on': ringing }"
            :title="t('ring.tip')"
            @click="ringPhone"
          >
            <BellRing class="h-[18px] w-[18px]" :stroke-width="1.9" />
          </button>
          <!-- Switch to another already-paired phone. Keeps this one connected
               while it looks, so a cancelled or fruitless switch leaves the
               link exactly as it was. -->
          <button
            v-if="peers.length > 1 || peerSwitchScanning"
            class="vx-ring"
            :disabled="peerSwitchScanning"
            :title="t('peers.switch_tip')"
            @click="startPeerSwitch"
          >
            <Loader2 v-if="peerSwitchScanning" class="h-[18px] w-[18px] animate-spin" />
            <TabletSmartphone v-else class="h-[18px] w-[18px]" :stroke-width="1.9" />
          </button>
        </div>
        <div class="flex items-center gap-2">
          <span
            class="vx-dot"
            :class="phoneOnline ? 'bg-primary vx-pulse' : phoneConnecting ? 'bg-amber-400 vx-pulse' : 'bg-muted-foreground'"
          />
          <span class="text-[13px] text-[hsl(var(--card-foreground)/0.82)]">
            {{ phoneOnline ? t("peers.connected") : phoneConnecting ? t("peers.connecting") : t("peers.offline") }}
          </span>
        </div>
        <div class="h-px bg-white/[0.06]" />
        <div class="flex items-center justify-between">
          <div class="flex items-center gap-1.5">
            <component
              :is="batteryIcon(primaryPeerState?.battery ?? null, primaryPeerState?.charging ?? false)"
              class="h-[18px] w-[18px]"
              :class="batteryClass(primaryPeerState?.battery ?? null, primaryPeerState?.charging ?? false)"
            />
            <span class="text-[13px] font-medium" :class="batteryClass(primaryPeerState?.battery ?? null, primaryPeerState?.charging ?? false)">
              {{ primaryPeerState?.battery != null ? primaryPeerState.battery + "%" : "—" }}
            </span>
          </div>
          <span v-if="primaryPeerState?.charging" class="text-xs text-muted-foreground">Charging</span>
        </div>
        <!-- Switch results: several candidates → pick one; none → say so.
             Rendered inside the card so it reads as being about this device. -->
        <div v-if="peerSwitchCandidates.length > 0" class="flex flex-col gap-2">
          <div class="h-px bg-white/[0.06]" />
          <div class="text-xs text-muted-foreground">{{ t("peers.switch_pick") }}</div>
          <button
            v-for="c in peerSwitchCandidates"
            :key="c.peer_static_pub"
            class="vx-chip justify-between"
            @click="choosePeer(c.peer_static_pub)"
          >
            <span class="truncate">{{ c.name || t("device.android") }}</span>
            <span class="text-[11px] text-muted-foreground">{{ c.rssi }} dBm</span>
          </button>
          <button class="text-xs text-muted-foreground hover:underline" @click="abortPeerSwitch">
            {{ t("peers.switch_cancel") }}
          </button>
        </div>
        <div v-else-if="peerSwitchNoneFound" class="flex flex-col gap-2">
          <div class="h-px bg-white/[0.06]" />
          <div class="text-xs text-muted-foreground">{{ t("peers.switch_none") }}</div>
        </div>
        <div v-else-if="peerSwitchScanning" class="flex flex-col gap-2">
          <div class="h-px bg-white/[0.06]" />
          <div class="text-xs text-muted-foreground">{{ t("peers.switch_scanning") }}</div>
          <button class="text-xs text-muted-foreground hover:underline" @click="abortPeerSwitch">
            {{ t("peers.switch_cancel") }}
          </button>
        </div>
        <!-- mt gives the absolute "Experimental" corner badges headroom above -->
        <div v-if="phoneOnline" class="mt-1.5 flex flex-wrap items-center gap-3">
          <!-- Always "Share screen", never a stop toggle: the mirror window now
               carries its own close button, and it tears the whole session down.
               This used to flip to "Stop sharing" because the old native window
               had no working X of its own. -->
          <button
            class="vx-chip relative"
            :class="{ 'vx-chip--live': mirrorActive }"
            :disabled="mirrorStarting"
            @click="startMirror"
          >
            <Loader2 v-if="mirrorStarting" class="h-3.5 w-3.5 animate-spin" />
            <MonitorUp v-else class="h-3.5 w-3.5" />
            {{ t("mirror.share_screen") }}
            <!-- Screen mirror ships Experimental in v1 (heavy GStreamer deps). -->
            <span class="vx-tag absolute -top-1.5 right-2">{{ t("mirror.experimental") }}</span>
          </button>
          <button class="vx-chip relative" :class="{ 'vx-chip--live': cameraOn }" @click="toggleCamera">
            <Video class="h-3.5 w-3.5" />
            {{ t("mirror.use_as_webcam") }}
            <!-- Continuity camera ships Experimental in v1 (v4l2loopback dep). -->
            <span class="vx-tag absolute -top-1.5 right-2">{{ t("mirror.experimental") }}</span>
          </button>
          <button v-if="cameraOn" class="vx-chip" @click="flipCamera">
            <SwitchCamera class="h-3.5 w-3.5" />
            {{ cameraFacing === "front" ? t("mirror.cam_front") : t("mirror.cam_back") }}
          </button>
        </div>
      </div>

      <!-- EARBUDS -->
      <button
        v-if="!activeEarbuds"
        class="vx-card flex flex-col items-center justify-center gap-2 py-7 text-center hover:border-primary/40"
        @click="openEarbudsPicker"
      >
        <span class="vx-icon"><Plus class="h-5 w-5" /></span>
        <span class="text-sm font-medium">{{ t("earbuds.add") }}</span>
        <span class="text-xs text-muted-foreground">{{ t("earbuds.add_hint") }}</span>
      </button>

      <div
        v-else
        class="vx-card flex flex-col gap-3.5"
        :class="{ 'opacity-60': isSwitching }"
        @contextmenu.prevent="earbudsMenuOpen = true"
      >
        <div class="flex items-center gap-3">
          <span class="vx-icon"><Headphones class="h-[22px] w-[22px]" /></span>
          <div class="min-w-0">
            <div class="text-[15px] font-semibold truncate">{{ activeEarbuds.name }}</div>
            <div class="mt-px text-xs text-muted-foreground">{{ t("device.earbuds") }}</div>
          </div>
        </div>
        <div class="flex items-center gap-2">
          <span class="vx-dot" :class="activeEarbuds.connected ? 'bg-primary shadow-[0_0_8px_hsl(var(--primary)/0.6)]' : 'bg-muted-foreground'" />
          <span class="text-[13px] text-[hsl(var(--card-foreground)/0.82)]">{{ earbudsStatus }}</span>
        </div>
        <div class="flex items-center gap-1.5">
          <component
            :is="batteryIcon(activeEarbuds.battery ?? null, false)"
            class="h-[18px] w-[18px]"
            :class="batteryClass(activeEarbuds.battery ?? null, false)"
          />
          <span class="text-[13px] font-medium" :class="batteryClass(activeEarbuds.battery ?? null, false)">
            {{ activeEarbuds.battery != null ? activeEarbuds.battery + "%" : "—" }}
          </span>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.vx-card {
  @apply rounded-[20px] border border-white/[0.07] p-[18px] text-left transition-colors;
  background: linear-gradient(180deg, hsl(var(--secondary)), hsl(var(--card)));
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.45), inset 0 1px 0 rgba(255, 255, 255, 0.03);
}
.vx-icon {
  @apply flex h-[42px] w-[42px] shrink-0 items-center justify-center rounded-xl border border-white/[0.06] bg-white/[0.05];
  color: #e8eaed;
}
.vx-dot {
  @apply h-2 w-2 shrink-0 rounded-full;
}
.vx-chip {
  @apply inline-flex items-center gap-1.5 rounded-full border border-white/[0.08] bg-white/[0.05] px-[13px] py-2 text-[12.5px] font-medium transition-colors hover:bg-white/[0.09] hover:text-foreground disabled:opacity-50;
  color: #d4d6db;
}
.vx-chip--live {
  @apply border-primary/40 bg-primary/[0.14] text-primary;
}
/* Find-My ring button — theme-safe tints (foreground/primary alpha) so it reads
   in light mode too; pulses while a ring was just requested. */
.vx-ring {
  @apply flex h-9 w-9 shrink-0 items-center justify-center rounded-full transition-colors;
  color: hsl(var(--muted-foreground));
  border: 1px solid hsl(var(--border));
  background: hsl(var(--foreground) / 0.04);
}
.vx-ring:hover {
  color: hsl(var(--foreground));
  background: hsl(var(--foreground) / 0.08);
}
.vx-ring--on {
  color: hsl(var(--primary));
  border-color: hsl(var(--primary) / 0.4);
  background: hsl(var(--primary) / 0.14);
  animation: vx-ring-pulse 0.5s ease-in-out infinite;
}
@keyframes vx-ring-pulse {
  0%, 100% { transform: scale(1); }
  50% { transform: scale(1.12); }
}
</style>
