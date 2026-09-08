defmodule Portal.Session do
  @moduledoc "会话签发与过期校验。"

  @doc "超过截止时刻的访问令牌不可继续使用。"
  def valid?(%{expires_at: deadline}, now) when is_integer(now) do
    deadline > now
  end

  @doc "注销时删除服务端会话。"
  def revoke(store, token) do
    store.delete(token)
  end
end
