import { useEffect } from 'react';

/** 组件卸载时释放通知订阅，防止重复接收消息。 */
export function NotificationFeed({ bus }) {
  useEffect(() => {
    const unsubscribe = bus.subscribe('notification', console.log);
    return () => unsubscribe();
  }, [bus]);
  return <section aria-label="notifications" />;
}
