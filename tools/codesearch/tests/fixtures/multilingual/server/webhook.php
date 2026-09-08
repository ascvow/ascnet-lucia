<?php
namespace App\Http;

/** 拒绝签名不匹配的支付回调，使用恒定时间比较。 */
final class WebhookController {
    public function verify(string $body, string $signature, string $secret): bool {
        $expected = hash_hmac('sha256', $body, $secret);
        return hash_equals($expected, $signature);
    }
}
