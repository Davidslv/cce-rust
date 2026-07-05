class Case < ApplicationRecord
  belongs_to :assignee, class_name: "User"
  has_many :comments, dependent: :destroy
  has_one :summary
  validates :title, presence: true
  scope :open, -> { where(closed: false) }
  enum status: { open: 0, closed: 1 }

  def close!
    update!(closed: true)
    log_event
  end
end
