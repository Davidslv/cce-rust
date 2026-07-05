module Billing
  # The billing engine exposes a single charge calculation used by the app.
  def self.charge(amount)
    amount * 100
  end
end
