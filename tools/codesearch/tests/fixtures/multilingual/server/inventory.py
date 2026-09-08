from fastapi import FastAPI, HTTPException

app = FastAPI()

@app.post('/inventory/reserve')
async def reserve_stock(sku: str, quantity: int):
    """库存不足时拒绝预占，成功后扣减可用数量。"""
    available = await repository.available(sku)
    if quantity <= 0 or available < quantity:
        raise HTTPException(status_code=409, detail='insufficient stock')
    return await repository.decrement(sku, quantity)
