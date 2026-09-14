<script setup lang="ts">
// Left sidebar — the single place to switch sections. A wider, labelled rail
// (collapsible to an icon strip) with a sliding active highlight, translated
// from the Vortex design system. Glass surface + emerald accent; the page
// content lives beside it in AppShell.
import { computed, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useI18n } from "vue-i18n";
import {
  ChevronsLeft,
  FolderOpen,
  MessageSquare,
  MonitorSmartphone,
  NotebookText,
  Phone,
  Settings,
  Users,
} from "lucide-vue-next";
// The logo asset is kept at 128px on purpose. It is drawn at 30px here and
// at most 56px anywhere else, and the webview has no compositing (see the
// WEBKIT_DISABLE_DMABUF_RENDERER note in src-tauri/src/main.rs), so every
// repaint anywhere in the window re-samples this image. At the original
// 512px that single rescale cost ~60% of a core while the connection dot
// was pulsing; at 128px it is ~16x less work and the same pixels on screen.
import logo from "@/assets/vortex_logo.png";
import { unreadConversations } from "@/composables/useMessages";

const route = useRoute();
const router = useRouter();
const { t } = useI18n();

// Default to the slim icon rail — it expands on the collapse toggle.
const collapsed = ref(true);

// One flat list so the highlight can slide across every entry (Settings
// included). `to: "/"` is the device dashboard; the rest start-match.
const items = computed(() => [
  { key: "home", icon: MonitorSmartphone, to: "/", label: t("nav.home") },
  { key: "contacts", icon: Users, to: "/contacts", label: t("nav.contacts") },
  { key: "recents", icon: Phone, to: "/recents", label: t("nav.recents") },
  { key: "messages", icon: MessageSquare, to: "/messages", label: t("nav.messages") },
  { key: "notes", icon: NotebookText, to: "/notes", label: t("nav.notes") },
  { key: "phone-files", icon: FolderOpen, to: "/phone-files", label: t("nav.phoneFiles") },
  { key: "settings", icon: Settings, to: "/settings", label: t("nav.settings") },
]);

const isActive = (to: string) =>
  to === "/" ? route.path === "/" : route.path.startsWith(to);

// Index of the active item drives the sliding highlight's offset. -1 (no
// match, e.g. /clipboard) hides it by parking it off-list.
const activeIndex = computed(() => items.value.findIndex((it) => isActive(it.to)));

const ITEM_STRIDE = 46; // 42px row + 4px gap

function go(to: string) {
  if (route.path !== to) router.push(to);
}
</script>

<template>
  <aside
    class="flex flex-col shrink-0 overflow-hidden border-r border-border backdrop-blur-2xl px-4 pt-[22px] pb-3"
    :style="{
      width: collapsed ? '76px' : '236px',
      background: 'hsl(var(--card) / 0.72)',
      transition: 'width .42s cubic-bezier(.22,1,.36,1)',
    }"
  >
    <!-- Logo + wordmark. The logo stays left-anchored so it never jumps; only
         the wordmark fades + slides right as the rail collapses. -->
    <div class="flex items-center justify-start gap-[11px] px-2 pt-1">
      <img
        :src="logo"
        alt="Vortex"
        class="h-[30px] w-[30px] shrink-0 rounded-md object-cover"
        style="filter: drop-shadow(0 0 5px hsl(var(--primary) / 0.45))"
      />
      <span
        class="text-[18px] font-semibold tracking-[-0.3px] whitespace-nowrap transition-[opacity,transform] duration-300"
        :class="collapsed ? 'opacity-0 translate-x-2' : 'opacity-100 translate-x-0'"
      >Vortex</span>
    </div>

    <!-- Nav with a sliding active highlight -->
    <nav class="relative mt-[30px] flex flex-col gap-1">
      <div
        v-show="activeIndex >= 0"
        class="absolute inset-x-0 top-0 h-[42px] rounded-xl z-0"
        :style="{
          background: 'hsl(var(--foreground) / 0.06)',
          transform: `translateY(${activeIndex * ITEM_STRIDE}px)`,
          transition: 'transform .44s cubic-bezier(.22,1,.36,1)',
        }"
      />

      <button
        v-for="it in items"
        :key="it.key"
        :title="it.label"
        class="relative z-[1] flex h-[42px] items-center justify-start gap-3 rounded-xl px-3 font-medium transition-colors"
        :class="isActive(it.to) ? 'text-foreground' : 'text-muted-foreground hover:text-secondary-foreground'"
        @click="go(it.to)"
      >
        <span class="relative inline-flex shrink-0">
          <component :is="it.icon" :size="19" :stroke-width="1.8" />
          <span
            v-if="it.key === 'messages' && unreadConversations > 0"
            class="absolute -top-1.5 -right-1.5 flex h-4 min-w-[16px] items-center justify-center rounded-full bg-primary px-0.5 text-[9px] font-semibold text-primary-foreground"
          >{{ unreadConversations > 99 ? "99+" : unreadConversations }}</span>
        </span>
        <span
          class="whitespace-nowrap text-sm transition-[opacity,transform] duration-300"
          :class="collapsed ? 'opacity-0 translate-x-2' : 'opacity-100 translate-x-0'"
        >{{ it.label }}</span>
      </button>
    </nav>

    <div class="flex-1" />

    <!-- Collapse toggle (the bottom status card is intentionally omitted) -->
    <button
      :title="collapsed ? 'Expand' : 'Collapse'"
      class="flex h-10 items-center justify-start gap-3 rounded-xl px-3 text-muted-foreground transition-colors hover:text-secondary-foreground"
      @click="collapsed = !collapsed"
    >
      <ChevronsLeft
        :size="19"
        :stroke-width="1.9"
        class="shrink-0 transition-transform duration-[420ms]"
        :class="collapsed ? 'rotate-180' : ''"
        style="transition-timing-function: cubic-bezier(.22,1,.36,1)"
      />
      <span
        class="whitespace-nowrap text-sm transition-[opacity,transform] duration-300"
        :class="collapsed ? 'opacity-0 translate-x-2' : 'opacity-100 translate-x-0'"
      >Collapse</span>
    </button>
  </aside>
</template>
