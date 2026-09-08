<template><button @click="submit">登录</button></template>
<!-- <script>function pretendLogin() { return true; }</script> -->
<script setup data-note="x > y" lang="ts">
import { ref } from 'vue';
const password = ref('');
/** 密码不足八位时阻止表单发送。 */
async function submit() {
  if (password.value.length < 8) throw new Error('Password too short');
  return fetch('/session', { method: 'POST', body: password.value });
}
</script>
