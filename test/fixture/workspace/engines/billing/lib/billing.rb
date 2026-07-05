module Billing
  def self.charge(amount)
    { amount: amount, status: "charged" }
  end
end
