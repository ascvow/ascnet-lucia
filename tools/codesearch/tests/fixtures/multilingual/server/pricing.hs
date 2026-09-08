module Pricing where

-- 阶梯运费：免邮门槛以下按重量收费。
shippingCost :: Double -> Double -> Double
shippingCost amount weight
  | amount >= 100 = 0
  | otherwise = 5 + weight * 2

-- 会员等级不影响商品库存。
data Membership = Standard | Premium

type Amount = Double
