<script lang="ts">
  /**
   * Знак AIOWA: конверт и искра. Та же картинка, что в иконке приложения
   * (`src-tauri/icons/source/aiowa.svg`), обрезанная по скруглённому квадрату. Декоративный.
   */
  let { size = 20 }: { size?: number } = $props();
  const uid = $props.id();
</script>

<svg class="logo" width={size} height={size} viewBox="64 64 896 896" aria-hidden="true" focusable="false">
  <defs>
    <linearGradient id="{uid}-bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#2E8FE6" />
      <stop offset="1" stop-color="#0B4DA2" />
    </linearGradient>
  </defs>
  <rect x="64" y="64" width="896" height="896" rx="208" fill="url(#{uid}-bg)" />
  <rect x="184" y="400" width="576" height="400" rx="64" fill="#FFFFFF" />
  <path d="M232 452 L472 628 L712 452" fill="none" stroke="#1D66C0" stroke-width="52" stroke-linecap="round" stroke-linejoin="round" />
  <path d="M752 160 Q752 300 892 300 Q752 300 752 440 Q752 300 612 300 Q752 300 752 160 Z" fill="#FFC83D" stroke="#1E6BC6" stroke-width="28" stroke-linejoin="round" paint-order="stroke" />
</svg>

<style>
  .logo {
    flex: none;
    display: block;
  }
</style>
