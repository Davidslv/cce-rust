class Charge
  def process(amount)
    Billing.charge(amount)
  end
end
