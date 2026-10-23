<script setup lang="ts">
// 状态徽章：pending 黄 / approved 绿 / rejected、revoked 红
const props = defineProps<{ status: string }>()

const LABELS: Record<string, string> = {
  pending: '待审批',
  approved: '已批准',
  rejected: '已拒绝',
  revoked: '已吊销',
}
const label = () => LABELS[props.status] ?? props.status
const cls = () =>
  ['pending', 'approved', 'rejected', 'revoked'].includes(props.status)
    ? `badge-${props.status}`
    : 'badge-default'
</script>

<template>
  <span class="badge" :class="cls()">{{ label() }}</span>
</template>
