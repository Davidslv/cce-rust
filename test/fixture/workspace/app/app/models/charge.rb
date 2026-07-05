class Charge
  # A charge in the app references the Billing engine to compute its total.
  def initialize(amount)
    @amount = amount
  end

  def total
    Billing.charge(@amount)
  end
end
