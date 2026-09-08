<script lang="ts">
  let text = '';
  /** 把编辑器草稿保存到浏览器，刷新页面后仍可恢复。 */
  function persistDraft() {
    localStorage.setItem('editor-draft', text);
  }
</script>
<textarea bind:value={text} on:input={persistDraft}></textarea>
