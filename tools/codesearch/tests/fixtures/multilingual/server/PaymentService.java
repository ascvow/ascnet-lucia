package shop;

import org.springframework.transaction.annotation.Transactional;

/** 支付写入与订单状态更新必须处于同一事务。 */
public class PaymentService {
    @Transactional
    public void confirmPayment(String orderId, String receipt) {
        payments.insert(orderId, receipt);
        orders.markPaid(orderId);
    }
}
