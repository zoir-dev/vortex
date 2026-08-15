<script setup lang="ts">
import { onMounted, onUnmounted, ref, computed } from "vue";
import { useI18n } from "vue-i18n";
import { useRouter } from "vue-router";
import {
  Moon,
  Sun,
  Globe,
  ArrowLeft,
  Headphones,
  Bell,
  BellRing,
  Lock,
  LockOpen,
  ClipboardList,
  MousePointer2,
  FileDown,
} from "lucide-vue-next";
import { theme } from "@/lib/theme";
import { smartSwitchEnabled, setSmartSwitch } from "@/lib/smartSwitch";
import { notifMirrorShow, setNotifMirror, notifMirrorSend, setNotifSend } from "@/lib/notifMirror";
import {
  proximityAutoLock,
  proximityAutoUnlock,
  setProximityAutoLock,
  setProximityAutoUnlock,
  initProximity,
} from "@/lib/proximity";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { setLocale, LOCALES, type LocaleCode } from "@/lib/i18n";
import { cn } from "@/lib/utils";
import SettingsRow from "@/components/SettingsRow.vue";

const router = useRouter();
const { t, te, locale } = useI18n();

onMounted(() => {
  void initProximity();
  void invoke<boolean>("get_clipboard_sync")
    .then((v) => (clipboardSync.value = v))
    .catch(() => {});
  void invoke<boolean>("get_file_auto_accept")
    .then((v) => (fileAutoAccept.value = v))
    .catch(() => {});
  void invoke<boolean>("uc_running")
    .then((v) => (ucEnabled.value = v))
    .catch(() => {});
  void invoke<string>("uc_get_placement")
    .then((v) => (ucPlacement.value = v))
    .catch(() => {});
  // Universal Control can only fail once it is already running — the desktop
  // withholding input capture, the phone dropping off adb — so the switch has to
  // hear about it after the fact, or it sits on over something that stopped.
  void listen<string>("vortex:uc-stopped", (e) => {
    ucEnabled.value = false;
    ucError.value = e.payload;
  }).then((un) => (unlistenUc = un));
});

let unlistenUc: UnlistenFn | undefined;
onUnmounted(() => unlistenUc?.());

// Universal Control: the laptop cursor + keyboard cross the screen edge onto
// the phone (drives its native cursor). Toggling on arms the edge barrier;
// `ucPlacement` is which screen edge the phone sits past.
const ucEnabled = ref(false);
const ucPlacement = ref("right");
/// Why it stopped, when it stopped by itself. Empty while all is well.
const ucError = ref("");
// The backend reports failures as `<code>` or `<code>|<detail>` rather than an
// English sentence, so the reason can be said in the user's own language (see
// universal_control.rs). Anything whose code has no translation — the errors from
// further down, libei and zbus — is shown exactly as it arrived.
const ucErrorText = computed(() => {
  if (!ucError.value) return "";
  const cut = ucError.value.indexOf("|");
  const code = cut < 0 ? ucError.value : ucError.value.slice(0, cut);
  const detail = cut < 0 ? "" : ucError.value.slice(cut + 1);
  const key = `settings.uc_err_${code}`;
  if (!te(key)) return ucError.value;
  return detail ? `${t(key)} (${detail})` : t(key);
});
const UC_EDGES = computed(() =>
  (["left", "right", "top", "bottom"] as const).map((code) => ({
    code,
    label: t(`settings.uc_edge_${code}`),
  })),
);
// The placement is "<edge>" or "<edge>-<end>". Arming only one end leaves the
// rest of that edge to whatever else lives there — an auto-hiding dock loses it
// completely otherwise, because the compositor stops moving the pointer the
// moment our barrier catches it.
const ucEdge = computed(() => ucPlacement.value.split("-")[0] || "right");
const ucEnd = computed(() => ucPlacement.value.split("-")[1] ?? "");
const UC_ENDS = computed(() => {
  const ends =
    ucEdge.value === "top" || ucEdge.value === "bottom"
      ? (["left", "right"] as const)
      : (["top", "bottom"] as const);
  return [
    { code: "", label: t("settings.uc_end_whole") },
    ...ends.map((code) => ({ code, label: t(`settings.uc_corner_${code}`) })),
  ];
});
function setUniversalControl(v: boolean) {
  ucEnabled.value = v;
  ucError.value = "";
  void invoke(v ? "uc_start" : "uc_stop").catch((e) => {
    ucEnabled.value = !v;
    ucError.value = String(e);
  });
}
function pickUcPlacement(code: string) {
  ucPlacement.value = code;
  void invoke("uc_set_placement", { edge: code }).catch(() => {});
}
// Picking an edge drops any end with it — "bottom-right" means nothing once the
// edge is vertical — so the edge buttons set the bare edge and the end is chosen
// again after.
function pickUcEnd(code: string) {
  pickUcPlacement(code ? `${ucEdge.value}-${code}` : ucEdge.value);
}

// Phone↔laptop clipboard sync (P2). Default on. Text copied on either side
// mirrors to the other (laptop→phone auto; phone→laptop via the phone's
// Quick Settings tile, an Android background-read limitation).
const clipboardSync = ref(true);
function setClipboardSync(v: boolean) {
  clipboardSync.value = v;
  void invoke("set_clipboard_sync", { enabled: v }).catch(() => {});
}

// Files shared FROM the phone normally raise an Accept / Decline banner. With
// this on they are saved straight away. Off by default (it drops a consent
// gate) and persisted on the Rust side, so the choice survives a restart.
// Revert the switch if the write fails — it must never show "on" over a
// setting that didn't stick.
const fileAutoAccept = ref(false);
function setFileAutoAccept(v: boolean) {
  const prev = fileAutoAccept.value;
  fileAutoAccept.value = v;
  void invoke("set_file_auto_accept", { enabled: v }).catch(() => {
    fileAutoAccept.value = prev;
  });
}

function pickLocale(code: LocaleCode) {
  // Local-only: laptop's language is independent of the phone's.
  setLocale(code);
}
function pickTheme(mode: "light" | "dark") {
  theme.value = mode;
}

// A segmented pill (language / theme): green & lifted when active, a soft
// outlined chip otherwise.
const pill = (active: boolean) =>
  cn(
    "flex-1 flex items-center justify-center gap-2 py-[11px] rounded-xl text-[13.5px] font-semibold cursor-pointer transition-all",
    active
      ? "bg-primary text-primary-foreground shadow-[0_4px_16px_rgba(46,204,113,0.3)]"
      : "bg-muted/40 border border-border text-foreground hover:bg-muted/70",
  );
</script>

<template>
  <div class="min-h-screen flex flex-col bg-background">
    <!-- Top bar with back arrow -->
    <header class="flex items-center gap-1 px-3 py-2.5 border-b border-border bg-card/40">
      <button
        class="h-9 w-9 rounded-md flex items-center justify-center hover:bg-accent transition-colors"
        @click="router.push('/')"
      >
        <ArrowLeft class="h-4 w-4" />
      </button>
      <h1 class="text-base font-semibold ml-1">{{ t("settings.title") }}</h1>
    </header>

    <!-- Body -->
    <main class="flex-1 overflow-auto">
      <div class="w-full px-7 pt-8 pb-14">
        <h1 class="text-[25px] font-semibold tracking-[-0.5px]">{{ t("settings.title") }}</h1>
        <p class="text-[13.5px] text-muted-foreground mt-1">{{ t("settings.subtitle") }}</p>

        <!-- APPEARANCE -->
        <div class="sec-label">{{ t("settings.sec_appearance") }}</div>
        <div class="rounded-[20px] border border-border bg-card overflow-hidden">
          <!-- language -->
          <div class="px-[18px] py-4">
            <div class="flex items-center gap-2.5 mb-3">
              <Globe class="h-[18px] w-[18px] text-muted-foreground" :stroke-width="1.8" />
              <span class="text-sm font-semibold text-foreground">{{ t("settings.language") }}</span>
            </div>
            <div class="flex gap-2.5">
              <button
                v-for="l in LOCALES"
                :key="l.code"
                :class="pill(locale === l.code)"
                @click="pickLocale(l.code)"
              >
                {{ l.label }}
              </button>
            </div>
          </div>
          <div class="h-px bg-border/60" />
          <!-- theme -->
          <div class="px-[18px] py-4">
            <div class="flex items-center gap-2.5 mb-3">
              <component :is="theme === 'dark' ? Moon : Sun" class="h-[18px] w-[18px] text-muted-foreground" :stroke-width="1.9" />
              <span class="text-sm font-semibold text-foreground">{{ t("settings.theme") }}</span>
            </div>
            <div class="flex gap-2.5">
              <button :class="pill(theme === 'dark')" @click="pickTheme('dark')">
                <Moon class="h-4 w-4" :stroke-width="1.9" />{{ t("settings.theme_dark") }}
              </button>
              <button :class="pill(theme === 'light')" @click="pickTheme('light')">
                <Sun class="h-4 w-4" :stroke-width="1.9" />{{ t("settings.theme_light") }}
              </button>
            </div>
          </div>
        </div>

        <!-- CONTINUITY -->
        <div class="sec-label">{{ t("settings.sec_continuity") }}</div>
        <div class="rounded-[20px] border border-border bg-card overflow-hidden">
          <SettingsRow
            :icon="Headphones"
            :title="t('settings.smart_switch')"
            :desc="t('settings.smart_switch_hint')"
            :model-value="smartSwitchEnabled"
            @update:model-value="setSmartSwitch"
          />
          <SettingsRow
            divider
            :icon="Bell"
            :title="t('settings.notif_mirror')"
            :desc="t('settings.notif_mirror_hint')"
            :model-value="notifMirrorShow"
            @update:model-value="setNotifMirror"
          />
          <SettingsRow
            divider
            :icon="BellRing"
            :title="t('settings.notif_send')"
            :desc="t('settings.notif_send_hint')"
            :model-value="notifMirrorSend"
            @update:model-value="setNotifSend"
          />
          <SettingsRow
            divider
            :icon="ClipboardList"
            :title="t('settings.clipboard_sync')"
            :desc="t('settings.clipboard_sync_hint')"
            :model-value="clipboardSync"
            @update:model-value="setClipboardSync"
          />
          <SettingsRow
            divider
            :icon="FileDown"
            :title="t('settings.file_auto_accept')"
            :desc="t('settings.file_auto_accept_hint')"
            :model-value="fileAutoAccept"
            @update:model-value="setFileAutoAccept"
          />
          <SettingsRow
            divider
            :icon="MousePointer2"
            :title="t('settings.uc')"
            :tag="t('mirror.experimental')"
            :desc="t('settings.uc_hint')"
            :model-value="ucEnabled"
            @update:model-value="setUniversalControl"
          />
          <!-- Experimental for a reason, and the reasons are all environmental —
               worth stating up front rather than as a failure later. -->
          <p class="px-[18px] pb-4 -mt-1.5 text-[11.5px] leading-relaxed text-muted-foreground">
            {{ t("settings.uc_needs") }}
          </p>
          <!-- why it switched itself off: no portal on this desktop, no adb, a
               barrier the compositor would not arm -->
          <p
            v-if="ucErrorText"
            class="px-[18px] pb-4 -mt-1 text-xs leading-relaxed text-destructive"
          >
            {{ ucErrorText }}
          </p>
          <!-- phone placement: which screen edge the phone sits past -->
          <div v-if="ucEnabled" class="px-[18px] py-4 border-t border-border/60">
            <div class="flex items-center gap-2.5 mb-3">
              <MousePointer2 class="h-[18px] w-[18px] text-muted-foreground" :stroke-width="1.8" />
              <span class="text-sm font-semibold text-foreground">
                {{ t("settings.uc_placement") }}
              </span>
            </div>
            <div class="flex gap-2.5">
              <button
                v-for="e in UC_EDGES"
                :key="e.code"
                :class="pill(ucEdge === e.code)"
                @click="pickUcPlacement(e.code)"
              >
                {{ e.label }}
              </button>
            </div>
            <div class="mt-2 flex flex-wrap gap-2">
              <button
                v-for="e in UC_ENDS"
                :key="e.code || 'full'"
                :class="pill(ucEnd === e.code)"
                @click="pickUcEnd(e.code)"
              >
                {{ e.label }}
              </button>
            </div>
          </div>
        </div>

        <!-- PRIVACY & PROXIMITY -->
        <div class="sec-label">{{ t("settings.sec_privacy") }}</div>
        <div class="rounded-[20px] border border-border bg-card overflow-hidden">
          <SettingsRow
            :icon="Lock"
            :title="t('settings.proximity_lock')"
            :desc="t('settings.proximity_lock_hint')"
            :model-value="proximityAutoLock"
            @update:model-value="setProximityAutoLock"
          />
          <SettingsRow
            divider
            :icon="LockOpen"
            :title="t('settings.proximity_unlock')"
            :desc="t('settings.proximity_unlock_hint')"
            :model-value="proximityAutoUnlock"
            @update:model-value="setProximityAutoUnlock"
          />
        </div>

        <p class="text-center mt-7 text-[11.5px] text-muted-foreground/70">
          Vortex · {{ t("settings.footer") }}
        </p>
      </div>
    </main>
  </div>
</template>

<style scoped>
.sec-label {
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 1.2px;
  text-transform: uppercase;
  color: hsl(var(--muted-foreground));
  margin: 26px 0 11px 4px;
}
</style>
