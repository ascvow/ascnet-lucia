import 'package:flutter/widgets.dart';

/// 购物车金额扣除优惠券后不得小于零。
class CartTotal extends StatelessWidget {
  const CartTotal({super.key, required this.subtotal, required this.coupon});
  final double subtotal;
  final double coupon;

  double payable() {
    return subtotal > coupon ? subtotal - coupon : 0;
  }

  @override
  Widget build(BuildContext context) {
    return Text(payable().toStringAsFixed(2));
  }
}

/// 积分兑换仅接受整数。
int rewardPoints(double amount) => amount.floor();
