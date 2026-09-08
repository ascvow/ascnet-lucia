package gateway

import "sync"

// 请求配额耗尽时拒绝继续访问，互斥锁保护并发扣减。
type Limiter struct {
    mu sync.Mutex
    remaining int
}

func (l *Limiter) Allow() bool {
    l.mu.Lock()
    defer l.mu.Unlock()
    if l.remaining <= 0 { return false }
    l.remaining--
    return true
}
