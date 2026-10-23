<script setup lang="ts">
import { ref } from 'vue'
import { useRouter } from 'vue-router'
import { adminApi, ApiError, setAdminToken } from '../../shared/api'

const router = useRouter()
const password = ref('')
const submitting = ref(false)
const errorMsg = ref('')

async function submit() {
  if (!password.value) {
    errorMsg.value = '请输入管理密码'
    return
  }
  submitting.value = true
  errorMsg.value = ''
  try {
    const r = await adminApi.login(password.value)
    setAdminToken(r.session_token)
    router.replace('/registrations')
  } catch (e) {
    if (e instanceof ApiError) {
      errorMsg.value =
        e.status === 401 ? '密码错误' : e.status === 429 ? '尝试次数过多，请稍后再试' : `登录失败：${e.message}`
    } else {
      errorMsg.value = '登录失败：网络错误'
    }
  } finally {
    submitting.value = false
  }
}
</script>

<template>
  <div class="login-wrap">
    <form class="card login-card" @submit.prevent="submit">
      <div class="logo">
        <span class="title">Remote CMD</span>
        <span class="sub">管理端登录</span>
      </div>
      <input
        v-model="password"
        class="input"
        type="password"
        placeholder="管理密码"
        autocomplete="current-password"
        autofocus
      />
      <button class="btn btn-primary" type="submit" :disabled="submitting">
        {{ submitting ? '登录中…' : '登 录' }}
      </button>
      <p v-if="errorMsg" class="error-text">{{ errorMsg }}</p>
    </form>
  </div>
</template>

<style scoped lang="scss">
.login-wrap {
  height: 100%;
  display: flex;
  align-items: center;
  justify-content: center;
}

.login-card {
  width: 360px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.logo {
  display: flex;
  flex-direction: column;
  gap: 4px;
  margin-bottom: 6px;
  text-align: center;

  .title {
    font-size: 20px;
    font-weight: 700;
    color: var(--primary);
  }

  .sub {
    font-size: 13px;
    color: var(--text-dim);
  }
}

.error-text {
  color: var(--danger);
  margin: 0;
  text-align: center;
  font-size: 13px;
}
</style>
