from auth import verify_password

def process_payment(amount, currency):
    return {"amount": amount, "currency": currency, "status": "ok"}

def refund_payment(payment_id):
    return {"payment_id": payment_id, "status": "refunded"}
